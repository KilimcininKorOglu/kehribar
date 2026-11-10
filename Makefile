.PHONY: build run test lint fmt up down logs

build:
	cargo build --release --locked

run:
	cargo run

test:
	cargo test --locked

lint:
	cargo fmt --check
	cargo clippy --locked --all-targets -- -D warnings
	lizard --languages rust --CCN 10 --warnings_only src

fmt:
	cargo fmt

up:
	docker compose up -d --build

down:
	docker compose down

logs:
	docker compose logs -f app
