use std::time::Instant;

fn batch(count: usize) -> Vec<(usize, Vec<u8>)> {
    (0..count)
        .map(|sequence| (sequence, vec![sequence as u8; 64]))
        .collect()
}

fn front_remove(mut packets: Vec<(usize, Vec<u8>)>) -> u64 {
    let mut digest = 0u64;
    for expected in 0..packets.len() {
        let (sequence, payload) = packets.remove(0);
        assert_eq!(sequence, expected);
        digest = digest
            .wrapping_mul(31)
            .wrapping_add(sequence as u64 ^ payload[0] as u64);
    }
    digest
}

fn linear_consume(packets: Vec<(usize, Vec<u8>)>) -> u64 {
    let mut digest = 0u64;
    for (expected, (sequence, payload)) in packets.into_iter().enumerate() {
        assert_eq!(sequence, expected);
        digest = digest
            .wrapping_mul(31)
            .wrapping_add(sequence as u64 ^ payload[0] as u64);
    }
    digest
}

fn main() {
    let count = 50_000;

    let started = Instant::now();
    let old_digest = front_remove(batch(count));
    let old_ms = started.elapsed().as_secs_f64() * 1_000.0;

    let started = Instant::now();
    let new_digest = linear_consume(batch(count));
    let linear_ms = started.elapsed().as_secs_f64() * 1_000.0;

    assert_eq!(old_digest, new_digest);
    println!(
        "DRAIN_REGRESSION_PASS packets={count} digest={new_digest} \
         old_ms={old_ms:.3} linear_ms={linear_ms:.3} speedup={:.1}x",
        old_ms / linear_ms,
    );
}
