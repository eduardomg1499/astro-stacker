fn main() {
    let w = 100;
    let h = 100;
    let large_buf = vec![0u8; w * h * 9];
    println!("Checking if image::RgbImage::from_raw panics...");
    let img = image::RgbImage::from_raw(w as u32, h as u32, large_buf);
    match img {
        Some(_) => println!("It returned SOME"),
        None => {
            println!("It returned NONE, unwrap will PANIC");
            // let _ = img.unwrap(); // This would panic normally
        }
    }
}
