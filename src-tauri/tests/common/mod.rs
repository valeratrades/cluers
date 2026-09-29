use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vad")
}

pub fn load(name: &str, rate: u32) -> Vec<f32> {
    let mut r = hound::WavReader::open(dir().join(format!("{name}_{rate}.wav"))).unwrap();
    assert_eq!(r.spec().sample_rate, rate);
    assert_eq!(r.spec().channels, 1);
    r.samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect()
}

pub fn truth(name: &str) -> Vec<(u64, u64)> {
    std::fs::read_to_string(dir().join(format!("{name}.truth")))
        .unwrap()
        .lines()
        .map(|l| {
            let (a, b) = l.split_once(' ').unwrap();
            (a.parse().unwrap(), b.parse().unwrap())
        })
        .collect()
}
