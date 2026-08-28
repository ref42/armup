# armup

`armup` is a Windows-focused CLI for setting up an embedded Cortex-M toolchain.
It downloads and installs the supported tools into one root directory, then can
add them to the current user's `Path`.

It installs:

- `arm-none-eabi-gcc`
- `clangd`
- `cmake`
- `ninja`
- `probe-rs`
- `xPack OpenOCD`

Ready to go:

```bash
armup install -a --root D:\Embedded_Toolchain -j 24
```

Update installed tools:

```bash
armup update -a --root D:\Embedded_Toolchain -j 24
```
