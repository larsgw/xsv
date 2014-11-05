use std::default::Default;
use std::fmt;
use std::from_str::{FromStr, from_str};
use std::io::{mod, File};
use std::os;
use std::str;

use csv::{mod, ByteString};
use csv::index::Indexed;
use stats::{Commute, OnlineStats, MinMax, Unsorted, merge_all};

use CliResult;
use config::{Config, Delimiter};
use select::{SelectColumns, Selection};
use util;

static USAGE: &'static str = "
Computes basic statistics on CSV data.

Basic statistics includes mean, median, mode, standard deviation, max and
min values. Note that some statistics are expensive to compute, so they must
be enabled explicitly. By default, the following statistics are reported for
*every* column in the CSV data: mean, max, min and standard deviation.

Computing statistics on a large file can be made much faster if you create
an index for it first with 'xsv index'.

Usage:
    xsv stats [options] [<input>]

stats options:
    -s, --select <arg>     Select a subset of columns to compute stats for.
                           See 'xsv select --help' for the format details.
                           This is provided here because piping 'xsv select'
                           into 'xsv stats' will disable the use of indexing.
    --mode                 Show the mode.
                           This requires storing all CSV data in memory.
    --cardinality          Show the cardinality.
                           This requires storing all CSV data in memory.
    --median               Show the median.
                           This requires storing all CSV data in memory.
    --nulls                Include NULLs in the population size for computing
                           mean and standard deviation.
    -j, --jobs <arg>       The number of jobs to run in parallel.
                           This works better when the given CSV data has
                           an index already created. Note that a file handle
                           is opened for each job.
                           When set to '0', the number of jobs is set to the
                           number of CPUs detected.
                           [default: 0]

Common options:
    -h, --help             Display this message
    -o, --output <file>    Write output to <file> instead of stdout.
    -n, --no-headers       When set, the first row will NOT be interpreted
                           as column names. i.e., They will be included
                           in statistics.
    -d, --delimiter <arg>  The field delimiter for reading CSV data.
                           Must be a single character. [default: ,]
";

#[deriving(Clone, Decodable)]
struct Args {
    arg_input: Option<String>,
    flag_select: SelectColumns,
    flag_mode: bool,
    flag_cardinality: bool,
    flag_median: bool,
    flag_nulls: bool,
    flag_jobs: uint,
    flag_output: Option<String>,
    flag_no_headers: bool,
    flag_delimiter: Delimiter,
}

pub fn run(argv: &[&str]) -> CliResult<()> {
    let args: Args = try!(util::get_args(USAGE, argv));

    let mut wtr = try!(io| Config::new(args.flag_output.clone()).writer());
    let (headers, stats) = try!(match try!(args.rconfig().indexed()) {
        None => args.sequential_stats(),
        Some(idx) => {
            if args.flag_jobs == 1 {
                args.sequential_stats()
            } else {
                args.parallel_stats(idx)
            }
        }
    });
    let stats = args.stats_to_records(stats);

    try!(csv| wtr.write(args.stat_headers().into_iter()));
    for (header, stat) in headers.iter().zip(stats.into_iter()) {
        let row = vec![header[]].into_iter()
                                .chain(stat.iter().map(|f| f.as_bytes()));
        try!(csv| wtr.write_bytes(row));
    }
    Ok(())
}

impl Args {
    fn sequential_stats(&self)
                       -> CliResult<(Vec<ByteString>, Vec<Stats>)> {
        let mut rdr = try!(io| self.rconfig().reader());
        let (headers, sel) = try!(self.sel_headers(&mut rdr));
        let stats = try!(self.compute(&sel, rdr.byte_records()));
        Ok((headers, stats))
    }

    fn parallel_stats(&self, idx: Indexed<io::File, io::File>)
                     -> CliResult<(Vec<ByteString>, Vec<Stats>)> {
        use std::comm::channel;
        use std::sync::TaskPool;

        let mut rdr = try!(io| self.rconfig().reader());
        let (headers, sel) = try!(self.sel_headers(&mut rdr));

        let chunk_size = idx.count() as uint / self.njobs();
        let nchunks = util::num_of_chunks(idx.count() as uint, chunk_size);

        let mut pool = TaskPool::new(self.njobs(), || { proc(_) () });
        let (send, recv) = channel();
        for i in range(0, nchunks) {
            let (send, args, sel) = (send.clone(), self.clone(), sel.clone());
            pool.execute(proc(_) {
                let mut idx = args.rconfig().indexed().unwrap().unwrap();
                idx.seek((i * chunk_size) as u64).unwrap();
                let it = idx.csv().byte_records().take(chunk_size);
                send.send(args.compute(&sel, it).unwrap());
            });
        }
        drop(send);
        Ok((headers, merge_all(recv.iter()).unwrap()))
    }

    fn stats_to_records(&self, stats: Vec<Stats>) -> Vec<Vec<String>> {
        use std::comm::channel;
        use std::sync::TaskPool;

        let mut records = Vec::from_elem(stats.len(), vec![]);
        let mut pool = TaskPool::new(self.njobs(), || { proc(_) () });
        let mut results = vec![];
        for mut stat in stats.into_iter() {
            let (tx, rx) = channel();
            results.push(rx);
            pool.execute(proc(_) { tx.send(stat.to_record()); });
        }
        for (i, rx) in results.into_iter().enumerate() {
            records[i] = rx.recv();
        }
        records
    }

    fn compute<I: Iterator<csv::CsvResult<Vec<ByteString>>>>
              (&self, sel: &Selection, mut it: I)
              -> CliResult<Vec<Stats>> {
        let mut stats = self.new_stats(sel.len());
        for row in it {
            let row = try!(csv| row);
            for (i, field) in sel.select(row[]).enumerate() {
                stats[i].add(field);
            }
        }
        Ok(stats)
    }

    fn sel_headers<R: Reader>(&self, rdr: &mut csv::Reader<R>)
                  -> CliResult<(Vec<ByteString>, Selection)> {
        let headers = try!(csv| rdr.byte_headers());
        let sel = try!(str| self.rconfig().selection(headers[]));
        Ok((sel.select(headers[]).map(ByteString::from_bytes).collect(), sel))
    }

    fn rconfig(&self) -> Config {
        Config::new(self.arg_input.clone())
               .delimiter(self.flag_delimiter)
               .no_headers(self.flag_no_headers)
               .select(self.flag_select.clone())
    }

    fn njobs(&self) -> uint {
        if self.flag_jobs == 0 { os::num_cpus() } else { self.flag_jobs }
    }

    fn new_stats(&self, record_len: uint) -> Vec<Stats> {
        Vec::from_elem(record_len, Stats::new(WhichStats {
            include_nulls: self.flag_nulls,
            range: true,
            dist: true,
            cardinality: self.flag_cardinality,
            median: self.flag_median,
            mode: self.flag_mode,
        }))
    }

    fn stat_headers(&self) -> Vec<String> {
        let mut fields = vec![
            "field", "type", "min", "max", "mean", "stddev",
        ];
        if self.flag_median { fields.push("median"); }
        if self.flag_mode { fields.push("mode"); }
        if self.flag_cardinality { fields.push("cardinality"); }
        fields.into_iter().map(|s| s.to_string()).collect()
    }
}

#[deriving(Clone, Eq, PartialEq, Show)]
struct WhichStats {
    include_nulls: bool,
    range: bool,
    dist: bool,
    cardinality: bool,
    median: bool,
    mode: bool,
}

impl Commute for WhichStats {
    fn merge(&mut self, other: WhichStats) {
        assert_eq!(*self, other);
    }
}

#[deriving(Clone)]
struct Stats {
    typ: FieldType,
    minmax: Option<TypedMinMax>,
    online: Option<OnlineStats>,
    mode: Option<Unsorted<ByteString>>,
    median: Option<Unsorted<f64>>,
    which: WhichStats,
}

impl Stats {
    fn new(which: WhichStats) -> Stats {
        let (mut minmax, mut online) = (None, None);
        let (mut mode, mut median) = (None, None);
        if which.range { minmax = Some(Default::default()); }
        if which.dist { online = Some(Default::default()); }
        if which.mode || which.cardinality { mode = Some(Default::default()); }
        if which.median { median = Some(Default::default()); }
        Stats {
            typ: Default::default(),
            minmax: minmax,
            online: online,
            mode: mode,
            median: median,
            which: which,
        }
    }

    fn add(&mut self, sample: &[u8]) {
        let sample_type = FieldType::from_sample(sample);
        self.typ.merge(sample_type);

        let t = self.typ;
        self.minmax.as_mut().map(|v| v.add(t, sample));
        self.mode.as_mut().map(|v| v.add(ByteString::from_bytes(sample)));
        match self.typ {
            TUnknown => {}
            TNull => {}
            TUnicode => {}
            TFloat => {
                if sample_type.is_null() {
                    if self.which.include_nulls {
                        self.online.as_mut().map(|v| { v.add_null(); });
                    }
                } else {
                    let n = from_bytes::<f64>(sample).unwrap();
                    self.median.as_mut().map(|v| { v.add(n); });
                    self.online.as_mut().map(|v| { v.add(n); });
                }
            }
            TInteger => {
                if sample_type.is_null() {
                    if self.which.include_nulls {
                        self.online.as_mut().map(|v| { v.add_null(); });
                    }
                } else {
                    let n = from_bytes::<f64>(sample).unwrap();
                    self.median.as_mut().map(|v| { v.add(n as f64); });
                    self.online.as_mut().map(|v| { v.add(n); });
                }
            }
        }
    }

    fn to_record(&mut self) -> Vec<String> {
        let typ = self.typ;
        let mut pieces = vec![];
        let empty = || "".to_string();

        pieces.push(self.typ.to_string());
        match self.minmax.as_ref().and_then(|mm| mm.show(typ)) {
            Some(mm) => { pieces.push(mm.0); pieces.push(mm.1); }
            None => { pieces.push(empty()); pieces.push(empty()); }
        }
        if !self.typ.is_number() {
            pieces.push(empty()); pieces.push(empty());
        } else {
            match self.online {
                Some(ref v) => {
                    pieces.push(v.mean().to_string());
                    pieces.push(v.stddev().to_string());
                }
                None => { pieces.push(empty()); pieces.push(empty()); }
            }
        }
        match self.median.as_mut().and_then(|v| v.median()) {
            None => {
                if self.which.median {
                    pieces.push(empty());
                }
            }
            Some(v) => { pieces.push(v.to_string()); }
        }
        match self.mode.as_mut() {
            None => {
                if self.which.mode {
                    pieces.push(empty());
                }
                if self.which.cardinality {
                    pieces.push(empty());
                }
            }
            Some(ref mut v) => {
                if self.which.mode {
                    let lossy: |ByteString| -> String =
                        |s| String::from_utf8_lossy(s[]).into_string();
                    let mode = v.mode().map(lossy).unwrap_or("N/A".to_string());
                    pieces.push(mode);
                }
                if self.which.cardinality {
                    pieces.push(v.cardinality().to_string());
                }
            }
        }
        pieces
    }
}

impl Commute for Stats {
    fn merge(&mut self, other: Stats) {
        self.typ.merge(other.typ);
        self.minmax.merge(other.minmax);
        self.online.merge(other.online);
        self.mode.merge(other.mode);
        self.median.merge(other.median);
        self.which.merge(other.which);
    }
}

#[deriving(Clone, PartialEq)]
enum FieldType {
    TUnknown,
    TNull,
    TUnicode,
    TFloat,
    TInteger,
}

impl FieldType {
    fn from_sample(sample: &[u8]) -> FieldType {
        if sample.is_empty() {
            return TNull;
        }
        let string = match str::from_utf8(sample) {
            None => return TUnknown,
            Some(s) => s,
        };
        if let Some(_) = from_str::<i64>(string) { return TInteger; }
        if let Some(_) = from_str::<f64>(string) { return TFloat; }
        TUnicode
    }

    fn is_number(&self) -> bool {
        *self == TFloat || *self == TInteger
    }

    fn is_null(&self) -> bool {
        *self == TNull
    }
}

impl Commute for FieldType {
    fn merge(&mut self, other: FieldType) {
        *self = match (*self, other) {
            (TUnicode, TUnicode) => TUnicode,
            (TFloat, TFloat) => TFloat,
            (TInteger, TInteger) => TInteger,
            // Null does not impact the type.
            (TNull, any) | (any, TNull) => any,
            // There's no way to get around an unknown.
            (TUnknown, _) | (_, TUnknown) => TUnknown,
            // Integers can degrate to floats.
            (TFloat, TInteger) | (TInteger, TFloat) => TFloat,
            // Numbers can degrade to Unicode strings.
            (TUnicode, TFloat) | (TFloat, TUnicode) => TUnicode,
            (TUnicode, TInteger) | (TInteger, TUnicode) => TUnicode,
        };
    }
}

impl Default for FieldType {
    // The default is the most specific type.
    // Type inference proceeds by assuming the most specific type and then
    // relaxing the type as counter-examples are found.
    fn default() -> FieldType { TInteger }
}

impl fmt::Show for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            TUnknown => write!(f, "Unknown"),
            TNull => write!(f, "NULL"),
            TUnicode => write!(f, "Unicode"),
            TFloat => write!(f, "Float"),
            TInteger => write!(f, "Integer"),
        }
    }
}

/// TypedMinMax keeps track of minimum/maximum values for each possible type
/// where min/max makes sense.
#[deriving(Clone)]
struct TypedMinMax {
    strings: MinMax<ByteString>,
    integers: MinMax<i64>,
    floats: MinMax<f64>,
}

impl TypedMinMax {
    fn add(&mut self, typ: FieldType, sample: &[u8]) {
        if sample.is_empty() {
            return;
        }
        self.strings.add(ByteString::from_bytes(sample));
        match typ {
            TUnicode | TUnknown => {}
            TFloat => {
                let n = str::from_utf8(sample[])
                            .and_then(|s| from_str::<f64>(s))
                            .unwrap();
                self.floats.add(n);
                self.integers.add(n as i64);
            }
            TInteger => {
                let n = str::from_utf8(sample[])
                            .and_then(|s| from_str::<i64>(s))
                            .unwrap();
                self.integers.add(n);
            }
            _ => unreachable!(),
        }
    }

    fn show(&self, typ: FieldType) -> Option<(String, String)> {
        match typ {
            TUnicode | TUnknown => {
                match (self.strings.min(), self.strings.max()) {
                    (Some(min), Some(max)) => {
                        let min = String::from_utf8_lossy(min[]).to_string();
                        let max = String::from_utf8_lossy(max[]).to_string();
                        Some((min, max))
                    }
                    _ => None
                }
            }
            TInteger => {
                match (self.integers.min(), self.integers.max()) {
                    (Some(min), Some(max)) => {
                        Some((min.to_string(), max.to_string()))
                    }
                    _ => None
                }
            }
            TFloat => {
                match (self.floats.min(), self.floats.max()) {
                    (Some(min), Some(max)) => {
                        Some((min.to_string(), max.to_string()))
                    }
                    _ => None
                }
            }
            _ => unreachable!(),
        }
    }
}

impl Default for TypedMinMax {
    fn default() -> TypedMinMax {
        TypedMinMax {
            strings: Default::default(),
            integers: Default::default(),
            floats: Default::default(),
        }
    }
}

impl Commute for TypedMinMax {
    fn merge(&mut self, other: TypedMinMax) {
        self.strings.merge(other.strings);
        self.integers.merge(other.integers);
        self.floats.merge(other.floats);
    }
}

fn from_bytes<T: FromStr>(bytes: &[u8]) -> Option<T> {
    str::from_utf8(bytes).and_then(from_str)
}