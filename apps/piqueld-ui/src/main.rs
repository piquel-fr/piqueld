/// Prints the package name and client version.
///
/// # Examples
///
/// ```
/// let version = piqueld_client::version();
/// println!("piqueld-ui {version}");
/// ```

fn main() {
    println!("piqueld-ui {}", piqueld_client::version());
}
