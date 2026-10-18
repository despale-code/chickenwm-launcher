<img src="icon.png" width="96" align="right" alt="">

# ChickenWM Launcher

Fuzzy app launcher with its own card design. Also a calculator, currency converter, clipboard history, shell-command runner, window switcher and power menu.

Part of [ChickenWM](https://github.com/despale-code/chickenwm), a Hyprland-style tiling desktop for
Windows. Each program lives in its own repository; the ChickenWM repository ties them together
and has the full documentation and the installer.

This project is vibecoded: written with an AI assistant.

## Building

Windows 10 or 11 (64-bit) and Rust 1.85 or newer:

```
cargo build --release
```

It depends on [chickenwm-core](https://github.com/despale-code/chickenwm-core), which Cargo fetches from GitHub.

## License

MIT, see [LICENSE](LICENSE).
