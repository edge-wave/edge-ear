//! Prints what an ONNX model expects and produces.
//!
//! Used to confirm a model's contract before code is written against
//! it. Model interfaces drift between versions, and a repackaged copy
//! may not match the one the project distributes.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: probe_model <model.onnx>")?;
    let session = ort::session::Session::builder()?.commit_from_file(&path)?;

    println!("model: {path}");
    println!("inputs:");
    for outlet in session.inputs() {
        println!("  {:<14} {:?}", outlet.name(), outlet.dtype());
    }
    println!("outputs:");
    for outlet in session.outputs() {
        println!("  {:<14} {:?}", outlet.name(), outlet.dtype());
    }
    Ok(())
}
