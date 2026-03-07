pub fn init() -> Result<(), Box<dyn std::error::Error>> {
    gix::open("")?;
    Ok(())
}
