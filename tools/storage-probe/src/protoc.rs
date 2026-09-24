fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", protoc_bin_vendored::protoc_bin_path()?.display());
    Ok(())
}
