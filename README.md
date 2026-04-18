<p align="center">
  <img src="docs/hero.png" alt="Rook - local-first AI with a memory graph that grows" width="100%" />
</p>

<h1 align="center">Rook</h1>

<p align="center">
  <b>Rook is a local-first AI client with a memory graph that actually grows. Bring your own model. Your data never leaves your machine.</b>
</p>

<p align="center">
  <a href="https://github.com/ayronny14-alt/Rook/releases/latest">
    <img src="https://img.shields.io/badge/Download-Rook%20for%20Windows-C86E3D?style=for-the-badge&logo=windows&logoColor=white" alt="Download Rook for Windows" />
  </a>
</p>

<p align="center">
  <a href="https://github.com/ayronny14-alt/Rook/releases">All releases</a> •
  <a href="https://github.com/ayronny14-alt/Rook/issues">Report an issue</a>
</p>

---

## Install

1. **Download** `Rook-Setup-<version>.exe` from the [latest release](https://github.com/ayronny14-alt/Rook/releases/latest).
2. **Run the installer.** It installs to `%LOCALAPPDATA%\Programs\Rook` by default (no admin required) and drops a Start Menu + Desktop shortcut.
3. **Launch Rook.** On first run, open **Settings** and paste in your API key for whichever provider you want to use.

Auto-updates are handled by [electron-updater](https://www.electron.build/auto-update). When a new release is published here, Rook picks it up in the background and prompts you to install - no admin prompt, no server in the middle.

## Bring your own API key

Rook is a client, not a service. You plug in whichever model you want:

- **OpenAI-compatible endpoints** - OpenAI, Anthropic (via proxy), OpenRouter, DeepInfra, Groq, Together, etc.
- **Local models via Ollama** - `llama`, `mistral`, `phi`, `qwen`, `deepseek`, `codellama`, anything Ollama serves.

Your key lives in `%APPDATA%\Rook\.env` on your machine and is sent straight to the provider you chose. Rook does not phone home, does not proxy through a server, does not collect telemetry.

## SmartScreen warning

The first time you run the installer, Windows SmartScreen will likely say **"Windows protected your PC."** This is because Rook is not yet signed with an EV certificate - SmartScreen flags every unsigned installer until it has built enough reputation across machines.

To proceed:

1. Click **More info**.
2. Click **Run anyway**.

If you'd rather not, you can inspect the source and build from scratch:

```sh
git clone https://github.com/ayronny14-alt/Rook.git
cd Rook
npm run build
```

The installer appears in `dist/Rook-Setup-<version>.exe`.

## How it works

- **Frontend** - Electron + vanilla JS renderer.
- **Backend** - Rust IPC server over a Windows named pipe (`\\.\pipe\rook`).
- **Memory** - Local SQLite with three layers: graph (entities + relationships), object (summaries + facts), and embedding (semantic search). A native-Rust GraphSAGE-style trainer runs periodically to smooth embeddings across the neighborhood and predict new edges - no Python required.
- **Installer** - NSIS via `electron-builder`. Per-user install so updates are silent.

## Build from source

```sh
# Requires: Rust (stable), Node 20+, Windows 10/11 x64
git clone https://github.com/ayronny14-alt/Rook.git
cd Rook
npm run build              # builds rook.exe + packages the installer
```

Or during development:

```sh
npm run dev                # launches Electron against a cargo-run backend
```


## License

ISC.
