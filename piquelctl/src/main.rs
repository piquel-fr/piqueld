fn main() {
    match piquelctl::run() {
        Ok(_) => {}
        Err(err) => panic!("Error: {err:#}"),
    }
}
