# Nexus AI 開発ガイド

Nexus AIは、NexusOSの既存APIと権限付きIPCを使って、タスクの実行結果まで検証する
AI Operating Layerを目指しています。このページは**現在動く機能**と
**今後必要な機能**を区別した入口です。

## 現在の実装範囲

2026-09-15時点。読取専用Runtimeに加え、Context選択とopt-inのローカル推論を
実装しています。コンポーネント別の検証結果と共有ツリー全体の統合結果は区別します。

| 項目 | 状態 |
|---|---|
| Runtime | `no_std`、基本RuntimeとContextはヒープ割当なし。optional modelは割当を利用 |
| Service | 通常のユーザープロセスとして起動。Kernel内部に依存しない |
| Session | サービスチャネルごとに固定session 1。複数チャネルの共有管理は未実装 |
| IPC | 24バイト要求／40バイト応答、version 1、厳密な長さ検査 |
| Tool | `system.info`だけ実行。実際のuptimeとサービスthread IDを取得 |
| Permission | 固定allowlist。危険操作は確認要求／特権要求／拒否となり実行されない |
| Verification | 2回の観測を比較。時刻の単調性・期限・thread IDを確認 |
| Failure handling | 再試行上限、要求数制限、リプレイ拒否、切断処理 |
| Audit | 直近の構造化メタデータとserial診断。会話・秘密情報は保存しない |
| Context | session/task権限範囲、期限、byte/token上限、秘密情報除外、UTF-8、日本語入力を扱う一時的な選択処理 |
| Host tests | Contextタスクでmodel有効38件、無効27件成功。後述のタスク記録を参照 |
| QEMU | 実プロセス起動、IPC、拒否、観測、検証、権限ハンドル回収を確認 |
| Model | opt-inのTinyStories 260K実推論。小さな英語補完モデルであり、日本語会話モデルではない |
| AI GUI | `nexus-assist`にキーワード判定のウィンドウあり。モデルとは未接続 |

推論worker・読取専用service・GUIは別コンポーネントで、モデル接続済みMVPではありません。`system.info`はRAM使用量やCPU負荷を
推測せず、現行userspace APIが返せる値だけを扱います。

## 動作の流れ

```text
Guest probe（専用テストディスクのINIT.ELF）
  → 既存spawn serviceでAI.ELFを起動
  → session / request ID / tool IDを送信
  → wire検査 → 権限評価 → 実際のOS API呼び出し
  → 再観測・検証 → 構造化された応答
  → 拒否・切断・リソース回収を確認
  → PASS後もKernelが動作することを確認
```

`ConfirmRequired`は「承認された」の意味ではありません。現行サービスに承認を
与えるAPIはなく、危険操作は実行されません。将来の承認は、モデルから独立した
trusted broker／UIで扱います。

## ビルドと検証

リポジトリルートのWindows PowerShellで実行してください。環境要件は
[プロジェクトREADME](../../README.md#build-and-run)を参照してください。

```powershell
# ホスト側のロジック・権限・protocolテスト
cargo test --offline -p nexus-ai-core

# AIサービスとguest probeのELFビルド
cargo build --offline -p nexus-ai --target targets/x86_64-nexus-user.json -Zbuild-std=core,compiler_builtins,alloc -Zbuild-std-features=compiler-builtins-mem

# OS起動を含む実際の統合テスト
.\scripts\test-ai.ps1
```

テストごとに `build/ai-<GUID>/` を作成します。通常のESP・INIT.ELF・OSディスクは
置き換えません。結果は同ディレクトリの `serial.log` に残り、失敗・タイムアウトは
非ゼロ終了コードになります。正常終了時の確認文字列：

```text
ai-probe: PASS service IPC policy verification disconnect; kernel alive
```

テストはこの文字列だけでなく、その後のKernel monitor出力も要求します。
[TESTING.md](TESTING.md)に実行結果、失敗から修正した内容、再現コマンド、成果物の
SHA-256を記録しています。この文書は最初のRuntime検証の履歴です。最新Contextの
38テスト・Clippy・独立チェックアウトのQEMU結果は
[ASTRA-CONTEXT-001](../../.ai_collaboration/tasks/ASTRA-CONTEXT-001.json)を参照してください。
共有ツリーの統合検証・レビューは別途追跡されています。これらはGUIのモデル接続検証ではありません。

実モデルの取得・ホスト実行・専用QEMU検証は
[ローカルモデル手順](../../tools/nexus-model/README.md)を参照してください。通常ビルドに
モデル資産は不要です。GUI単体の検証手順は `scripts/test-assist.ps1` にあります。

## 実装の入口

| 場所 | 責務 |
|---|---|
| [core](../../shared/nexus-ai/src/lib.rs) | Tool、Permission、Runtime、検証、状態、Audit |
| [context](../../shared/nexus-ai/src/context.rs) | 認可済み入力の選択、privacy、期限、token予算、出所管理 |
| [assistant](../../user/nexus-assist/src/main.rs) | キーワードによるtool選択、読取専用ファイル・machine情報のGUI |
| [wire](../../shared/nexus-ai/src/wire.rs) | 要求・応答のencode/decodeと妥当性検査 |
| [tests](../../shared/nexus-ai/src/tests.rs) | 拒否・期限・再試行・不正入力などのホストテスト |
| [service](../../user/nexus-ai/src/main.rs) | nexus-user API接続、waitset、IPC受付と応答 |
| [probe](../../user/nexus-ai/src/probe.rs) | 実OS上での別プロセス／権限／回収の検証 |
| [QEMU harness](../../scripts/test-ai.ps1) | ビルド、専用ディスク、起動、ログ判定、終了 |

Kernel／syscall ABIへの変更はありません。AI crateは既存のworkspace構成を利用します。
通常ビルドは `AI.ELF` と `ASSIST.ELF` を配置します。モデルworkerはopt-inで、
assistant GUIへの推論接続は未実装です。

## ドキュメント一覧

| 文書 | 読む場面 |
|---|---|
| [CURRENT_STATE](CURRENT_STATE.md) | OS監査、利用できるAPI、競合リスクを確認する |
| [AI_ARCHITECTURE](AI_ARCHITECTURE.md) | プロセス構成と責務の境界を理解する |
| [AI_ROADMAP](AI_ROADMAP.md) | 段階別の実装順と受入条件を確認する |
| [AI_TECHNICAL_DEBT](AI_TECHNICAL_DEBT.md) | TLS・推論・sandboxなどの制約を確認する |
| [AGENT_MODEL](AGENT_MODEL.md) | 現在のタスク状態と将来のAgent/DAG設計を確認する |
| [TOOL_API](TOOL_API.md) | tool IDと実行／拒否結果を調べる |
| [IPC](IPC.md) | バイナリ形式、応答コード、チャネルの制限を調べる |
| [PERMISSIONS](PERMISSIONS.md) / [AI_SECURITY](AI_SECURITY.md) | 権限評価と信頼境界を確認する |
| [SANDBOX](SANDBOX.md) | 現在の上限とOS側で必要な隔離を確認する |
| [CONTEXT](CONTEXT.md) / [MEMORY](MEMORY.md) | 実装済みContext選択と将来の永続memory設計を確認する |
| [MODEL_RUNTIME](MODEL_RUNTIME.md) | provider分離と実バックエンドの前提を確認する |
| [TESTING](TESTING.md) | 実際に通した検証と未検証範囲を確認する |
| [CONTRIBUTING](CONTRIBUTING.md) | 変更・レビュー・検証・協調の手順を確認する |

## 次の統合条件

- **System awareness:** `shared/nexus-machine` とassistant側の読取機能は存在します。
  RuntimeのContext adapterはまだ検証済みuptime/thread IDだけを扱うため、machine情報の
  接続時には権限・wire形式・観測の検証方法を合わせます。
- **Model:** 小さな英語補完workerをGUIへ接続する前に、応答待ち・取消し・生成テキストの
  安全な表示を検証します。実用的な日本語instruction modelと計算・メモリ予算は別途必要です。
  TLS 1.3とbrowser HTTPSは実装されていますが、remote model providerと認証情報管理は未実装です。
- **操作権限:** trusted broker、取消し、resource scope、OSが強制する実行上限を
  整備してからファイル変更やterminal実行を有効にします。
- **統合・検証:** Gemini 3.1 Proが担当。Claude Codeとの共有ツリーでビルド・QEMU・GUIを
  検証し、個別の成功結果だけで統合完了とは扱いません。

実装・レビュー依頼は [`.ai_collaboration/`](../../.ai_collaboration/README.md) に記録し、
STATEとロックを確認して進めます。将来設計の文書が存在しても、実装済みとは扱いません。
