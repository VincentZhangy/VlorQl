/// Quick compile-time size check for Expression.
#[test]
fn expression_size_fits_in_64_bytes() {
    // If Expression grows beyond 64 bytes, the static_assertion in
    // expressions.rs will fail.  This test is a runtime double-check.
    let sz = std::mem::size_of::<vlorql_core::schema::Expression>();
    assert!(sz <= 64, "Expression is {sz} bytes, expected ≤ 64");
}
