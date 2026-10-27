// 发布版不弹控制台窗口（Windows）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    gantry_desktop_lib::run();
}
