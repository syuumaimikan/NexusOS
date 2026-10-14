use std::{env, fs, path::Path};
fn main() {
    let arg = env::args().nth(1).expect("GSP bundle directory");
    let root = Path::new(&arg);
    let rm = fs::read(root.join("GSPRM.BIN")).expect("GSPRM.BIN");
    let ucodes = fs::read(root.join("UCODES.BIN")).expect("UCODES.BIN");
    nexus_nvidia::gsp::verify(&rm[..], false).expect("pinned GSP-RM");
    nexus_nvidia::gsp::verify(&ucodes[..], true).expect("pinned UCODES");
    let parsed = nexus_nvidia::gsp::Gsp::parse(&rm[..]).unwrap();
    println!(
        "PASS: pinned {} GB20x GSP-RM image={} signature={} bytes, external ucodes={} bytes; no execution",
        nexus_nvidia::VERSION,
        parsed.image.size,
        parsed.signature.size,
        ucodes.len()
    );
}
