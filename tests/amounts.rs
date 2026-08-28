use paymos_wallet::{format_units, parse_units};

#[test]
fn parses_human_decimals_into_raw_units() {
    assert_eq!(parse_units("1.5", 6).unwrap(), "1500000");
    assert_eq!(parse_units("1", 6).unwrap(), "1000000");
    assert_eq!(parse_units("0.000001", 6).unwrap(), "1");
    assert_eq!(parse_units("12", 0).unwrap(), "12");
}

#[test]
fn rejects_anything_that_is_not_a_non_negative_decimal() {
    for bad in ["-1", "abc", "", "1,5", "1.2.3", " "] {
        assert!(parse_units(bad, 6).is_err(), "should reject {bad:?}");
    }
    assert!(parse_units("1.1234567", 6).is_err(), "too many fractional digits");
}

#[test]
fn survives_amounts_a_double_would_round() {
    // 18 decimals: past what an IEEE double holds exactly, which is the reason these are strings.
    let raw = parse_units("123456789.123456789123456789", 18).unwrap();
    assert_eq!(raw, "123456789123456789123456789");
    assert_eq!(format_units(&raw, 18).unwrap(), "123456789.123456789123456789");
}

#[test]
fn formats_raw_units_back_dropping_trailing_zeros() {
    assert_eq!(format_units("1500000", 6).unwrap(), "1.5");
    assert_eq!(format_units("1000000", 6).unwrap(), "1");
    assert_eq!(format_units("1", 6).unwrap(), "0.000001");
    assert_eq!(format_units("12", 0).unwrap(), "12");
}

#[test]
fn round_trips() {
    for (human, dec) in [("1.5", 6), ("0.000001", 6), ("1000", 18), ("7", 0)] {
        assert_eq!(format_units(&parse_units(human, dec).unwrap(), dec).unwrap(), human);
    }
}
