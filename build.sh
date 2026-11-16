#!/usr/bin/env bash
# Сборка mpt.mp из исходников.
set -euo pipefail
cd "$(dirname "$0")"

export PATH="$HOME/.cargo/bin:$PATH"

cargo build --release
cp target/release/mpt-mp mpt.mp
chmod +x mpt.mp

echo
echo "Готово: $(pwd)/mpt.mp"
echo "Запуск: ./mpt.mp   или   mpt.mp (если ~/.local/bin в PATH)"
