//! A one-off dump of what the PDF writer actually emits, for settling test expectations.
//!
//! Not a test: the assertions that matter live in `pdf.rs` and `documents.rs`. This exists
//! because a dozen of them were written from a guess about the byte stream rather than from the
//! byte stream, and a guess about a binary format is worth exactly as much as it costs.
//!
//! `cargo run -p omnion-module-sales --example pdf_probe`

use omnion_module_sales::pdf::{encode_text, Document};

fn main() {
    for sample in ["—", "İğde Işığı Ltd.", "Širket Çözüm ülem", "0.00", "A—B"] {
        let (bytes, encoding) = encode_text(sample);
        println!(
            "{sample:?} -> {:02x?}  replaced={}",
            bytes, encoding.replaced
        );
    }

    // A short document's operators, so the x-position parsing can be read rather than imagined.
    let mut doc = Document::new("probe");
    doc.text(48.0, 9.0, false, "349.75");
    doc.text(300.0, 10.0, true, "407.76");
    doc.text(48.0, 9.0, false, "Subtotal");
    let bytes = doc.finish();
    let text = String::from_utf8_lossy(&bytes);
    println!("--- operators (each line, with the byte offset) ---");
    let mut cursor = 0usize;
    for line in text.split_inclusive('\n') {
        if line.contains("Tj") {
            println!("{:4}: {:?}", cursor, line.trim_end());
        }
        cursor += line.len();
    }
    println!("--- xref table, with each line's exact length ---");
    if let Some(at) = text.find("xref\n") {
        for line in text[at..].split_inclusive('\n').take(12) {
            println!("{:3} bytes: {:?}", line.len(), line);
        }
    }
}
