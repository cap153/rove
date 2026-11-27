set shell := ["bash", "-cu"]

gnu  := "x86_64-pc-windows-gnu"
msvc := "x86_64-pc-windows-msvc"

# 不带参数时显示帮助
default:
    @just --list

# femtovg 的 release 构建（gnu 目标）
rel:
    cargo build --release --target {{gnu}}

# Skia 的 release 构建（msvc 目标）
skia:
    cargo xwin build --release --target {{msvc}} --features skia
