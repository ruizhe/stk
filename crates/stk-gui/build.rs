fn main() {
    #[cfg(windows)]
    {
        winresource::WindowsResource::new()
            .set_icon("assets/stk-icon.ico")
            .compile()
            .expect("failed to embed the SSH Tunnel Keeper application icon");
    }
}
