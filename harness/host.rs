//! Run metadata shared by the benchmarks: the JSON output line and the host-load probes.

use std::hint::black_box;
use std::time::Instant;

pub struct Json(pub Vec<(String, String)>);

impl Json {
    pub fn num(&mut self, k: &str, v: impl std::fmt::Display) {
        self.0.push((k.to_string(), v.to_string()));
    }
    pub fn float(&mut self, k: &str, v: f64, decimals: usize) {
        self.0.push((k.to_string(), format!("{v:.decimals$}")));
    }
    pub fn text(&mut self, k: &str, v: &str) {
        let escaped: String = v
            .chars()
            .flat_map(|c| match c {
                '"' => vec!['\\', '"'],
                '\\' => vec!['\\', '\\'],
                c if c.is_control() => vec![],
                c => vec![c],
            })
            .collect();
        self.0.push((k.to_string(), format!("\"{escaped}\"")));
    }
    pub fn render(&self) -> String {
        let body: Vec<String> = self.0.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
        format!("{{{}}}", body.join(","))
    }
}

pub fn control_ms() -> f64 {
    let t = Instant::now();
    let mut x = black_box(0x9E37_79B9_7F4A_7C15u64);
    for _ in 0..100_000_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
    }
    black_box(x);
    t.elapsed().as_secs_f64() * 1e3
}

pub fn cpus_allowed() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("Cpus_allowed_list:")
                    .map(|v| v.trim().to_string())
            })
        })
        .unwrap_or_else(|| "unknown".into())
}

pub fn loadavg() -> f64 {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(f64::NAN)
}
