pub fn digits(n: u64) -> String {
    itoa::Buffer::new().format(n).to_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_dependency_setup_fetched_is_there() {
        assert_eq!(super::digits(118), "118");
    }
}
