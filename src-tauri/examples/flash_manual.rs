//! 独立闪光探针：脱离 Tauri 运行时直接调用 screen_flash，验证原生光圈窗
//! 在"纯 Rust + NSApp run"环境下能否上屏。用法：
//! `cargo run --example flash_manual -- <x> <y> <w> <h>`
//! （逻辑坐标，与 detect_local_screens 同空间；进程须手动结束。）

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<f64> = std::env::args()
        .skip(1)
        .map(|arg| arg.parse().expect("坐标必须是数字"))
        .collect();
    let (x, y, width, height) = (args[0], args[1], args[2], args[3]);
    println!("flashing logical ({x},{y}) {width}x{height}");
    unsafe {
        use objc2::runtime::AnyObject;
        use objc2::{class, msg_send};
        // 主线程要跑 NSApplication 才会派发主队列回调（create_flash 靠它执行）。
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setActivationPolicy: 1usize]; // accessory
        mykvm_lib::screen_flash::flash_logical_rect((x, y, width, height));
        let _: () = msg_send![app, run];
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macOS only");
}
