//! Puts the bebo icons into the program file.
fn main() {
    println!("cargo:rerun-if-changed=assets/app");
    // Windows shows the icon with the lowest id for the program file. bebo selects icon 1 or 2 for its window at the start.
    winresource::WindowsResource::new()
        .set_icon_with_id("assets/app/bebo-dark.ico", "1")
        .set_icon_with_id("assets/app/bebo-light.ico", "2")
        .compile()
        .expect("cannot add the icons");
}
