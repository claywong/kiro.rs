.PHONY: dev run build release clean test lint fmt ui ui-dev docker help

# 默认目标
help:
	@echo "Usage: make <target>"
	@echo ""
	@echo "开发:"
	@echo "  dev        启动前端 dev server + Rust 后端"
	@echo "  run        构建前端并运行 Rust 后端"
	@echo "  ui-dev     启动前端 dev server"
	@echo ""
	@echo "构建:"
	@echo "  ui         构建前端"
	@echo "  build      构建前端 + 后端（debug）"
	@echo "  release    构建前端 + 后端（release）"
	@echo "  docker     构建 Docker 镜像"
	@echo ""
	@echo "质量:"
	@echo "  test       运行测试"
	@echo "  lint       cargo clippy"
	@echo "  fmt        cargo fmt"
	@echo "  check      fmt + clippy + test"
	@echo ""
	@echo "其他:"
	@echo "  clean      清理构建产物"

# --- 前端 ---

ui:
	cd admin-ui && pnpm install && pnpm build

ui-dev:
	@echo "启动前端 dev server: http://localhost:5173/admin/"
	cd admin-ui && pnpm install && pnpm dev

# --- 后端 ---

dev:
	@echo "启动开发模式：前端 dev server + Rust 后端"
	@echo "前端访问地址请使用: http://localhost:5173/admin/"
	@echo "前端 /api 请求会代理到: http://localhost:8990"
	@cd admin-ui && pnpm install && pnpm dev & \
	RUST_LOG=kiro_rs::anthropic::cache_tracker=debug,kiro_rs=info cargo run --features sensitive-logs -- -c config/config.json --credentials config/credentials.json

run: ui
	cargo run --features sensitive-logs -- -c config/config.json --credentials config/credentials.json

build: ui
	cargo build

release: ui
	cargo build --release

# --- 质量 ---

test:
	cargo test

lint:
	cargo clippy -- -D warnings

fmt:
	cargo fmt

check: fmt lint test

# --- Docker ---

docker:
	docker build -t kiro-rs .

# --- 清理 ---

clean:
	cargo clean
	rm -rf admin-ui/dist admin-ui/node_modules
