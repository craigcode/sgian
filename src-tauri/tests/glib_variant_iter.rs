#![cfg(target_os = "linux")]

use glib::variant::ToVariant;

#[test]
fn string_variant_iterators_preserve_values_in_optimized_builds() {
    let values = ["first", "", "λ", "last"];
    let variant = values.to_variant();
    assert_eq!(
        variant.array_iter_str().unwrap().collect::<Vec<_>>(),
        values
    );
    assert_eq!(variant.array_iter_str().unwrap().next(), Some("first"));
    assert_eq!(variant.array_iter_str().unwrap().last(), Some("last"));
    assert_eq!(variant.array_iter_str().unwrap().nth(2), Some("λ"));
    assert_eq!(variant.array_iter_str().unwrap().next_back(), Some("last"));
    assert_eq!(variant.array_iter_str().unwrap().nth_back(2), Some(""));
    let mut iter = variant.array_iter_str().unwrap();
    assert_eq!(iter.next(), Some("first"));
    assert_eq!(iter.next_back(), Some("last"));
    assert_eq!(iter.collect::<Vec<_>>(), ["", "λ"]);
    let empty = Vec::<String>::new().to_variant();
    assert_eq!(empty.array_iter_str().unwrap().next(), None);
    assert_eq!(empty.array_iter_str().unwrap().next_back(), None);
}
