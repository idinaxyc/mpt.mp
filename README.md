# mpt.mp

Лёгкий модульный музыкальный плеер на Rust. Основное приложение работает с локальной музыкальной библиотекой, а дополнительные возможности подключаются через плагины.

## Получить готовую сборку

Готовые версии для Linux x86_64 и Windows x86_64 публикуются в [GitHub Releases](https://github.com/idinaxyc/mpt.mp/releases).

### Linux

В терминале:

```bash
curl -fsSL https://raw.githubusercontent.com/idinaxyc/mpt.mp/main/install.sh | bash
```

Установщик скачает последний Linux-релиз, распакует его в `~/.local/opt/mpt.mp` и создаст команду `~/.local/bin/mpt-mp`. Нужна работающая аудиосистема ALSA/PipeWire/PulseAudio.

### Windows

Откройте PowerShell:

```powershell
irm https://raw.githubusercontent.com/idinaxyc/mpt.mp/main/install.ps1 | iex
```

Приложение будет установлено в `%LOCALAPPDATA%\Programs\mpt.mp`. Откройте новое окно терминала после установки.

## Первый запуск

1. Запустите `mpt-mp`.
2. Откройте **FOLDERS** и добавьте папку с музыкой.
3. На вкладке **PLUGINS** включайте нужные дополнения.

## Плагины

Примеры плагинов находятся в каталоге `plugins/`. Документация формата и API находится в [plugins.md](plugins.md). Встроенные Python-плагины требуют Python 3.8 или новее. Перед включением стороннего плагина изучите его исходный код и разрешения.

## Сборка из исходников

Исходный код находится прямо в корне репозитория. Для локальной сборки клонируйте проект и выполните:

```bash
git clone https://github.com/idinaxyc/mpt.mp.git
cd mpt.mp
cargo build --locked --release
cargo test --locked
```

Для Linux также требуется пакет разработки ALSA, например `libasound2-dev` в Debian/Ubuntu.

## Релизы

Workflow GitHub Actions собирает Linux- и Windows-версии по тегу вида `v0.1.0`. После проверки сборок релиз можно создать так:

```bash
git tag v0.1.0
git push origin v0.1.0
```

## Лицензия

MIT. См. [LICENSE](LICENSE).
