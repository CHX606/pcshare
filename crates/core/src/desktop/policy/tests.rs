use super::*;

#[test]
fn congestion_downgrades_and_recovery_requires_sustained_evidence() {
    let mut p = Policy::new(Duration::from_millis(12));
    let slow = Duration::from_millis(240);
    for expected in [Some(Balanced), Some(Economy), None] {
        for _ in 0..3 {
            assert!(!p.observe(slow, slow, Duration::from_millis(120), Default::default()));
        }
        assert!(p.observe(slow, slow, Duration::from_millis(120), Default::default()));
        assert_eq!(p.tier, expected);
    }
    assert!(!p.probe(Duration::from_millis(25)));
    assert!(!p.probe(slow));
    assert!(!p.probe(Duration::from_millis(25)));
    assert!(p.probe(Duration::from_millis(25)));
    assert_eq!(p.tier, Some(Economy));
    for _ in 0..59 {
        assert!(!p.observe(
            Duration::from_millis(25),
            Duration::ZERO,
            Duration::from_millis(100),
            Default::default()
        ));
    }
    assert!(p.observe(
        Duration::from_millis(25),
        Duration::ZERO,
        Duration::from_millis(100),
        Default::default()
    ));
    assert_eq!(p.tier, Some(Balanced));
}

#[test]
fn an_isolated_spike_or_idle_screen_does_not_change_quality() {
    let mut p = Policy::new(Duration::from_millis(10));
    assert!(!p.observe(
        Duration::from_millis(900),
        SECOND,
        30 * SECOND,
        Default::default()
    ));
    assert!(!p.observe(
        Duration::from_millis(15),
        Duration::ZERO,
        2 * SECOND,
        Default::default()
    ));
    assert_eq!(p.tier, Some(High));
    assert_eq!(Policy::new(Duration::from_millis(350)).tier, None);
}

#[test]
fn accumulating_video_delay_triggers_adaptation_even_with_low_rtt() {
    let mut p = Policy::new(Duration::from_millis(12));
    for _ in 0..32 {
        p.observe(
            Duration::from_millis(12),
            Duration::from_millis(240),
            Duration::from_millis(16),
            Default::default(),
        );
    }
    assert_eq!(p.tier, Some(Balanced));
}

#[test]
fn sustained_high_rtt_and_queue_delay_skip_expensive_encoder_restarts() {
    let mut p = Policy::new(Duration::from_millis(12));
    for _ in 0..4 {
        p.observe(
            Duration::from_millis(290),
            Duration::from_millis(200),
            Duration::from_millis(130),
            Default::default(),
        );
    }
    assert_eq!(p.tier, None);
}

#[test]
fn low_bandwidth_estimate_on_an_idle_desktop_does_not_cost_quality() {
    let mut p = Policy::new(Duration::from_millis(12));
    let link = pcshare_proto::desktop::Delivery {
        capacity_bps: Some(500_000),
        loss_per_mille: 0,
    };
    for _ in 0..10 {
        assert!(!p.observe(
            Duration::from_millis(12),
            Duration::ZERO,
            Duration::from_millis(100),
            link
        ));
    }
    for _ in 0..5 {
        p.observe(
            Duration::from_millis(12),
            Duration::from_millis(80),
            Duration::from_millis(100),
            link,
        );
    }
    assert_eq!(p.tier, Some(Balanced));
}
