//! Print the variables SimGDB reads from an ELF: `cargo run --example
//! variables -- ELF`.
fn main() {
    let path = std::env::args().nth(1).expect("usage: variables ELF");
    let v = simgdb_elf::variables::variables(std::path::Path::new(&path)).unwrap();
    for var in &v.variables {
        println!("{:#010x} {:>5?} {:<24} {}", var.addr, var.size, var.name, v.name(var.ty));
    }
}
