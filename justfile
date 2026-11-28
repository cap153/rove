set shell := ["bash", "-cu"]

gnu  := "x86_64-pc-windows-gnu"
msvc := "x86_64-pc-windows-msvc"

[private]
default:
    @just --list

# femtovg 的 release 构建（gnu 目标）
rel:
    cargo build --release --target {{gnu}}

# Skia 的 release 构建（msvc 目标）
skia:
    cargo xwin build --release --target {{msvc}} --features skia

# 两个渲染器的 release 都编译（先 femtovg，再 Skia）
all: rel skia
