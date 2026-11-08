//! Prometheus's text exposition format, just the parts the metrics use.

use std::fmt::Write;

/// Label names and values for one series.
pub type Labels = Vec<(&'static str, String)>;

/// Cumulative buckets, as Prometheus histograms count them.
#[derive(Clone, Debug, PartialEq)]
pub struct Histogram {
    bounds: Vec<f64>,
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Histogram {
    pub fn new(bounds: &[f64]) -> Self {
        Histogram {
            bounds: bounds.to_vec(),
            counts: vec![0; bounds.len()],
            sum: 0.0,
            count: 0,
        }
    }

    pub fn observe(&mut self, value: f64) {
        for (bound, count) in self.bounds.iter().zip(&mut self.counts) {
            if value <= *bound {
                *count += 1;
            }
        }
        self.sum += value;
        self.count += 1;
    }
}

#[derive(Default)]
pub struct Writer {
    out: String,
}

impl Writer {
    pub fn counter(&mut self, name: &str, help: &str, series: impl IntoIterator<Item = (Labels, f64)>) {
        self.family(name, help, "counter", series);
    }

    pub fn gauge(&mut self, name: &str, help: &str, series: impl IntoIterator<Item = (Labels, f64)>) {
        self.family(name, help, "gauge", series);
    }

    pub fn histogram<'a>(&mut self, name: &str, help: &str, series: impl IntoIterator<Item = (Labels, &'a Histogram)>) {
        self.header(name, help, "histogram");
        for (labels, histogram) in series {
            for (bound, count) in histogram.bounds.iter().zip(&histogram.counts) {
                let mut with_bound = labels.clone();
                with_bound.push(("le", bound.to_string()));
                self.sample(&format!("{name}_bucket"), &with_bound, *count as f64);
            }
            let mut infinite = labels.clone();
            infinite.push(("le", "+Inf".to_owned()));
            self.sample(&format!("{name}_bucket"), &infinite, histogram.count as f64);
            self.sample(&format!("{name}_sum"), &labels, histogram.sum);
            self.sample(&format!("{name}_count"), &labels, histogram.count as f64);
        }
    }

    pub fn finish(self) -> String {
        self.out
    }

    fn family(&mut self, name: &str, help: &str, kind: &str, series: impl IntoIterator<Item = (Labels, f64)>) {
        self.header(name, help, kind);
        for (labels, value) in series {
            self.sample(name, &labels, value);
        }
    }

    fn header(&mut self, name: &str, help: &str, kind: &str) {
        let _ = writeln!(self.out, "# HELP {name} {help}");
        let _ = writeln!(self.out, "# TYPE {name} {kind}");
    }

    fn sample(&mut self, name: &str, labels: &Labels, value: f64) {
        self.out.push_str(name);
        if !labels.is_empty() {
            let pairs: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{}\"", escape(v))).collect();
            let _ = write!(self.out, "{{{}}}", pairs.join(","));
        }
        let _ = writeln!(self.out, " {value}");
    }
}

/// Label values escape backslashes, quotes and newlines.
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_families_with_escaped_labels_and_cumulative_buckets() {
        let mut histogram = Histogram::new(&[1.0, 5.0]);
        histogram.observe(0.5);
        histogram.observe(3.0);
        histogram.observe(9.0);
        let mut out = Writer::default();
        out.counter("calls_total", "Calls.", [(vec![("model", "a\"b\\c".to_owned())], 2.0)]);
        out.histogram("took_seconds", "Took.", [(vec![], &histogram)]);
        assert_eq!(
            out.finish(),
            "# HELP calls_total Calls.\n# TYPE calls_total counter\ncalls_total{model=\"a\\\"b\\\\c\"} 2\n\
             # HELP took_seconds Took.\n# TYPE took_seconds histogram\n\
             took_seconds_bucket{le=\"1\"} 1\ntook_seconds_bucket{le=\"5\"} 2\ntook_seconds_bucket{le=\"+Inf\"} 3\n\
             took_seconds_sum 12.5\ntook_seconds_count 3\n"
        );
    }
}
