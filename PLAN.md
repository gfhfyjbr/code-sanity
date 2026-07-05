# План: code-sanity для агентной санитизации кода

Дата подготовки: 2026-07-05

## 1. Короткий вывод

Идея резонная, но только если формулировать ее как **лексическую нормализацию и приватизационную санитизацию для снижения ложных срабатываний и утечек контекста**, а не как способ скрыть реальное поведение программы от модели.

Технически это реализуемо, но **хуков самих по себе недостаточно**. Надежная архитектура должна быть такой:

1. Есть реальный репозиторий, который является source of truth.
2. Есть санитизированное зеркало с тем же деревом файлов, но с замененными идентификаторами/строками/комментариями.
3. Есть индекс и база соответствий: оригинальный файл, санитизированный файл, хеши, замены, byte/line span mapping.
4. Агенты читают зеркало или специальный MCP/CLI read-tool.
5. Все edit/write/apply_patch проходят через bridge, который применяет изменения к зеркалу, переводит патч в координаты реального кода, применяет его к реальному файлу, затем пересанитизирует измененные файлы.
6. Hooks используются как адаптеры и guardrails: направить read/search к зеркалу, запретить прямые опасные обходы, завернуть shell output, синхронизировать после edit.

Сложность:

- MVP на статическом словаре и одном агенте: 5-8 рабочих дней.
- Рабочий multi-agent прототип для Codex + Claude Code + opencode: 2-4 недели.
- Устойчивый продуктовый уровень с model-based sanitizer, AST-aware mapping, конфликтами, concurrent sessions и rollback: 1-2 месяца.

Главный риск: не сама замена слов, а **точная обратная проекция edits** из санитизированного текста в реальный код при несовпадающих длинах строк, переименованиях, multiline replacements, форматтерах и параллельных изменениях.

## 2. Источники и факты из документации

### Codex hooks

Источник: https://developers.openai.com/codex/hooks

Важное:

- Codex hooks запускают детерминированные скрипты в lifecycle агента.
- Hook sources: `hooks.json` или inline `[hooks]` в `config.toml`; практические места: `~/.codex/hooks.json`, `~/.codex/config.toml`, `<repo>/.codex/hooks.json`, `<repo>/.codex/config.toml`.
- Есть trust review: non-managed command hooks нужно просмотреть и доверить через `/hooks`.
- События включают `PreToolUse`, `PostToolUse`, `PermissionRequest`, `UserPromptSubmit`, `Stop`, compact/subagent events.
- `PreToolUse` умеет перехватывать `Bash`, `apply_patch` и MCP tool calls. В документации прямо сказано, что это guardrail, а не полный enforcement boundary.
- `PreToolUse` не перехватывает все shell calls; новый `unified_exec` перехватывается неполностью. Также не перехватывает `WebSearch` и другие non-shell/non-MCP tools.
- Для `PreToolUse` Codex позволяет не только deny, но и rewrite supported tool call через `permissionDecision: "allow"` + `updatedInput`.
- Для `Bash` и `apply_patch` `updatedInput` должен содержать строковое поле `command`; для MCP tools это replacement arguments object.
- `PostToolUse` видит `tool_response`, но уже не может undo side effects. Можно заменить нормальную обработку результата feedback-ом через block/continue semantics, но это не полноценный output rewrite API.

Вывод для проекта:

- Codex-адаптер должен использовать hooks для `apply_patch` и MCP tools.
- Для read лучше дать MCP filesystem/proxy tool или запускать Codex из sanitized mirror.
- Попытка надежно санитизировать любые `cat`, `sed`, `rg`, `grep`, `nl` через Bash hooks будет неполной: часть shell-path может не перехватиться.

### Claude Code hooks

Источник: https://code.claude.com/docs/en/hooks

Важное:

- Hooks в Claude Code получают JSON context на stdin для command hooks или HTTP POST body для HTTP hooks.
- Есть события `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PostToolBatch`, `UserPromptSubmit`, `MessageDisplay`, `FileChanged`, `WorktreeCreate`, `WorktreeRemove`, `InstructionsLoaded` и другие.
- `PreToolUse` может блокировать tool call; пример в документации блокирует destructive Bash command через `permissionDecision: "deny"`.
- Hook locations: `~/.claude/settings.json`, `.claude/settings.json`, `.claude/settings.local.json`, managed policy settings, plugin `hooks/hooks.json`.
- Matcher filters есть для tool events; `if` использует permission rule syntax, например `Bash(git *)` или `Edit(*.ts)`.
- MCP tools выглядят как обычные tools в tool events, с именами вида `mcp__<server>__<tool>`.
- Hook handlers могут быть command, HTTP, MCP tool, prompt, agent.
- Документация показывает deny/context/control, но не дает такого же явного `updatedInput` rewrite контракта, как у Codex.

Вывод для проекта:

- Claude Code лучше подключать через MCP server и hooks-guardrails.
- Не стоит рассчитывать, что Claude hooks смогут прозрачно переписать любой read/edit tool call.
- Для строгого режима Claude нужно либо запускать в sanitized worktree, либо блокировать raw reads и требовать `code_sanity.read/search/apply_patch`.

### opencode plugins

Источник: https://opencode.ai/docs/plugins/

Важное:

- Plugins лежат в `.opencode/plugins/` или `~/.config/opencode/plugins/`, либо подключаются из npm через config.
- Plugin экспортирует функцию, получает context `{ project, client, $, directory, worktree }` и возвращает hooks object.
- Есть события `tool.execute.before`, `tool.execute.after`, `file.edited`, `file.watcher.updated`, `shell.env`, `session.*`, `permission.*` и другие.
- В примере `tool.execute.before` меняет `output.args.command` для bash.
- В примере `.env protection` plugin проверяет `input.tool === "read"` и `output.args.filePath.includes(".env")`, после чего бросает ошибку.

Вывод для проекта:

- opencode наиболее удобен для первого полноценного adapter prototype: plugin может перехватывать `read`, менять аргументы tool call и блокировать доступ.
- Для edit нужно проверить фактические tool names/args в installed opencode версии, но event model выглядит подходящим.

### CocoIndex

Источники:

- https://github.com/cocoindex-io/cocoindex
- https://cocoindex.io/docs/programming_guide/core_concepts/
- https://cocoindex.io/docs/connectors/localfs/
- https://cocoindex.io/docs/connectors/postgres/

Важное:

- CocoIndex позиционируется как incremental engine для свежего контекста AI agents.
- Core model: declarative state-driven sync. Source state -> transform -> target state.
- Incremental processing пересчитывает только изменения, отслеживает delta, intermediate states и target updates.
- Local filesystem connector умеет `walk_dir`, filtering через `PatternFilePathMatcher`, live file watching через `live=True`.
- Local filesystem target умеет `declare_file` / `declare_dir_target`, то есть можно строить mirror directory как target state.
- Postgres connector умеет source/target таблицы и pgvector, если нужен более мощный индекс.

Вывод для проекта:

- CocoIndex подходит как база для incremental indexing и генерации sanitized mirror.
- CocoIndex не решает самую тяжелую часть: точное преобразование edits из sanitized координат в original координаты. Для этого нужен отдельный patch-mapper.
- Для Rust-only MVP CocoIndex можно отложить, но для полноценной версии удобно использовать Python service с CocoIndex и Rust CLI/daemon как bridge.

## 3. Безопасная рамка продукта

Нужно заранее зафиксировать, что продукт не должен скрывать опасное поведение программы.

Разрешенные цели:

- убирать токсичные/нежелательные слова из имен, комментариев, тестовых fixtures;
- редактировать приватные доменные названия, имена клиентов, internal aliases;
- приводить грубые/провокационные идентификаторы к нейтральным;
- защищать секреты и персональные данные;
- уменьшать ложное semantic framing, когда название функции выглядит опаснее, чем фактическое поведение.

Запрещенные/плохие цели:

- маскировать вредоносную семантику;
- заменять dangerous API так, чтобы модель перестала понимать реальные side effects;
- подменять смысл логики;
- скрывать от reviewer/security scanner, что код делает на самом деле;
- отключать safety mechanisms агента.

Практическое правило:

- Можно менять лексику: identifiers, comments, test data, string literals по политике.
- Нельзя менять control flow, imports, API calls, syscall names, protocol semantics, cryptographic meaning, auth semantics.
- Если операция реально destructive/network/exfiltration/security-sensitive, агент должен видеть это как поведение, даже если имена переменных нормализованы.

## 4. Архитектура

### 4.1 Компоненты

```text
real repo
  |
  | index/sanitize
  v
.code-sanity/
  db.sqlite
  mirror/
  maps/
  journal/
  config.toml
  agent/
    codex/
    claude/
    opencode/

agent read/search/edit
  |
  v
hooks / plugin / MCP proxy
  |
  v
code-sanity daemon / CLI
  |
  +--> sanitized mirror
  +--> mapping DB
  +--> real repo patch applier
```

Основные бинарники/команды:

- `code-sanity init` - создать `.code-sanity/config.toml`, `.gitignore` entries, базовые hook configs.
- `code-sanity index` - первичная индексация real repo и генерация mirror.
- `code-sanity serve` - daemon для MCP/HTTP/plugin adapters.
- `code-sanity read <path>` - прочитать sanitized file.
- `code-sanity search <query>` - искать по sanitized mirror, возвращать sanitized paths/spans.
- `code-sanity apply-patch --sanitized <patch>` - принять patch against mirror, применить к real repo, обновить mirror.
- `code-sanity write --path <path> --sanitized-content <file>` - заменить sanitized file и back-project в real.
- `code-sanity sync` - пересканировать измененные real files.
- `code-sanity verify` - проверить hashes, mapping consistency, no drift.
- `code-sanity doctor --agent codex|claude|opencode` - проверить hooks/plugins/MCP installation.
- `code-sanity install-hooks --agent codex|claude|opencode` - сгенерировать configs.

### 4.2 Storage layout

```text
.code-sanity/
  config.toml
  db.sqlite
  mirror/
    src/main.rs
    Cargo.toml
  maps/
    src/main.rs.map.json
  journal/
    2026-07-05T20-10-00Z.patch.json
  logs/
    daemon.log
  tmp/
```

`.code-sanity/mirror/` должен быть исключен из git по умолчанию.

### 4.3 База данных

Минимальная SQLite schema:

```sql
files(
  id integer primary key,
  rel_path text unique not null,
  original_hash text not null,
  sanitized_hash text not null,
  original_size integer not null,
  sanitized_size integer not null,
  language text,
  updated_at text not null
);

replacements(
  id integer primary key,
  file_id integer not null,
  category text not null,
  original_text text not null,
  sanitized_text text not null,
  confidence real,
  policy_source text not null,
  stable_key text not null
);

spans(
  id integer primary key,
  file_id integer not null,
  replacement_id integer,
  original_start integer not null,
  original_end integer not null,
  sanitized_start integer not null,
  sanitized_end integer not null,
  original_line_start integer not null,
  sanitized_line_start integer not null
);

patch_journal(
  id integer primary key,
  session_id text,
  agent text,
  sanitized_patch text not null,
  original_patch text not null,
  status text not null,
  created_at text not null
);
```

Позже:

- `symbols` для AST-aware identifiers;
- `sessions` для lock ownership;
- `agent_events` для audit;
- `secrets` для irreversible redactions;
- `conflicts` для ручного resolve.

## 5. Санитизация

### 5.1 Pipeline

1. File discovery:
   - уважать `.gitignore`;
   - пропускать `.git/`, `target/`, `node_modules/`, binary files, lockfiles по config;
   - детектировать язык по extension и content sniffing.

2. Tokenization:
   - для MVP: regex tokenizer + tree-sitter там, где быстро доступно;
   - для нормального режима: tree-sitter для Rust/Python/JS/TS/Go + language-specific fallback.

3. Candidate detection:
   - статический словарь suspicious terms;
   - allowlist для слов, которые нельзя менять;
   - optional local/model-based classifier;
   - category: `identifier`, `comment`, `string_literal`, `doc`, `path`, `test_fixture`.

4. Replacement proposal:
   - отдельная модель предлагает нейтральную замену;
   - replacement должен проходить syntax constraints;
   - для identifiers: valid identifier for language;
   - для strings: не ломать формат, escaping, tests по возможности;
   - для comments/docs: сохранить смысл без провокационных слов.

5. Deterministic aliasing:
   - одинаковый original term в одном scope получает одинаковый alias;
   - stable salt хранится в `.code-sanity/config.toml`;
   - разные проекты не обязаны получать одинаковые aliases.

6. Rendering:
   - применить non-overlapping replacements справа налево по byte offsets;
   - сохранить line endings;
   - по возможности не менять количество строк;
   - записать mirror file и span map.

7. Verification:
   - sanitized file должен парситься тем же parser, если менялись identifiers/code tokens;
   - если невозможно, пометить файл `read-only sanitized` и не принимать edits без ручного resolve.

### 5.2 Что менять в MVP

Безопасный MVP:

- comments;
- doc comments;
- string literals только в тестах/fixtures;
- private identifiers внутри одного файла;
- local variable/function names при наличии parser support.

Не менять в MVP:

- public API names;
- filenames/module paths;
- imports/exports;
- serialized protocol strings;
- SQL queries;
- shell commands;
- env var names;
- feature flags;
- test snapshots, если нет snapshot update strategy.

### 5.3 Замены и длины

Есть два режима:

1. Length-preserving mode:
   - alias подбирается той же длины или padded;
   - проще patch mapping;
   - часто выглядит искусственно.

2. Span-mapped mode:
   - alias любой длины;
   - нужен полноценный byte span map;
   - это правильный режим для продукта.

Рекомендация: сразу делать span-mapped mode, но сохранять line count по умолчанию. Это резко снижает боль в hunks, compiler errors и диагностике.

## 6. Read/search модель

### 6.1 Лучший путь

Агенту нужно дать sanctioned tools:

- `code_sanity.read_file(path, range?)`
- `code_sanity.search(query, glob?)`
- `code_sanity.list_files(glob?)`
- `code_sanity.get_map(path, sanitized_span?)`
- `code_sanity.apply_patch(patch)`

Реализация:

- MCP server для Codex/Claude и любых MCP-aware агентов.
- opencode plugin может либо вызывать daemon HTTP API, либо напрямую менять tool args.
- Hooks заставляют агента использовать эти tools и блокируют raw access в strict mode.

### 6.2 Почему не только hooks

Обычный read может быть:

- native read tool;
- MCP filesystem read;
- shell `cat`;
- shell `sed -n`;
- shell `rg`;
- shell `nl`;
- IDE-side context loader;
- file upload/context ingest;
- LSP diagnostics;
- compiler output.

Ни один из трех hook API не дает универсальной гарантии "любой байт перед моделью пройдет через sanitizer".

Надежные варианты:

1. Запускать агента с `cwd = .code-sanity/mirror` и отдельным bridge для edits/tests.
2. Давать MCP/CLI tools и блокировать raw root reads best-effort hooks.
3. Использовать OS-level sandbox/FUSE/overlay FS, где агент физически видит только mirror.

Практический выбор:

- MVP: sanctioned MCP/CLI tools + hooks warnings/blocking.
- Strict mode: sanitized worktree / overlay FS.
- Enterprise mode: managed hooks + filesystem sandbox.

## 7. Edit/write модель

### 7.1 Нормальный flow

1. Агент читает sanitized file:

```text
real:      fn dangerous_word_parser() { ... }
sanitized: fn neutral_parser() { ... }
```

2. Агент делает patch against sanitized mirror:

```diff
-fn neutral_parser() {
+fn neutral_parser(input: &str) {
```

3. Bridge проверяет patch на mirror:

- `git apply --check` или встроенный diff parser;
- hunk context должен совпасть с текущим sanitized hash;
- если hash устарел, сначала `sync`.

4. Bridge строит original patch:

```diff
-fn dangerous_word_parser() {
+fn dangerous_word_parser(input: &str) {
```

5. Bridge применяет original patch к real repo:

- atomic write во временный файл + rename;
- backup или journal entry;
- lock per file;
- rollback при ошибке.

6. Bridge пересанитизирует измененные files:

- обновляет mirror;
- обновляет span map;
- пишет patch journal.

7. Hooks/PostToolUse сообщает агенту sanitized result.

### 7.2 Patch mapping algorithm

Нужны два уровня:

1. Простой mapping для unchanged regions:
   - span map хранит монотонное соответствие original byte ranges <-> sanitized byte ranges;
   - hunk coordinates переводятся через nearest span boundary.

2. Semantic replacement-aware mapping:
   - если edit касается sanitized alias, нужно понять: агент меняет alias как имя символа или просто surrounding text;
   - если меняет alias на новое имя, нужно решить, менять ли original symbol или только sanitized alias.

Правило MVP:

- Если edit целиком внутри replacement span, запрещать automatic apply и просить explicit rename command.
- Если edit пересекает replacement span, включать conflict mode.
- Если edit добавляет новый код рядом, применять автоматически.

Расширенный режим:

- `rename_sanitized_symbol(old_alias, new_alias)`:
  - обновляет replacement mapping;
  - опционально меняет original symbol, если пользователь разрешил;
  - пересанитизирует все references.

### 7.3 Conflict cases

Автоматически не применять:

- patch меняет sanitized alias на неизвестный alias;
- patch удаляет половину replacement span;
- hunk context совпадает в mirror, но original context стал другим;
- real file изменился снаружи после последней индексации;
- форматтер переставил блок так, что line mapping устарел;
- sanitized file не парсится после patch.

В conflict mode:

- сохранить sanitized patch в journal;
- показать минимальный conflict report;
- предложить `code-sanity resolve <journal-id>`.

## 8. Agent adapters

### 8.1 Codex adapter

Файлы:

```text
.codex/hooks.json
.codex/hooks/pre_tool_use.py
.codex/hooks/post_tool_use.py
```

Задачи:

- `PreToolUse` на `apply_patch|Edit|Write`:
  - перехватить patch;
  - если patch идет в `.code-sanity/mirror`, заменить input на command, который вызывает `code-sanity apply-patch`;
  - если patch идет прямо в real repo и strict mode включен, deny с объяснением.

- `PreToolUse` на MCP tools:
  - разрешать `mcp__code_sanity__read_file/search/apply_patch`;
  - блокировать raw `mcp__filesystem__read_file` для real root в strict mode.

- `PreToolUse` на `Bash`:
  - best-effort: rewrite obvious `cat src/foo.rs`, `sed -n ... src/foo.rs`, `rg term src/` на `code-sanity read/search`;
  - не считать это полной защитой.

- `PostToolUse`:
  - после successful apply вызвать `code-sanity sync --changed-only`;
  - для shell output в soft mode дать warning, если output содержит unsanitized terms.

Ограничения:

- Codex docs прямо предупреждают, что `PreToolUse` не полная enforcement boundary.
- Не все shell calls перехватываются.
- PostToolUse не может надежно заменить произвольный output, только повлиять на дальнейшее сообщение feedback-ом.

### 8.2 Claude Code adapter

Файлы:

```text
.claude/settings.json
.claude/hooks/pre_tool_use.py
.claude/hooks/post_tool_use.py
```

Задачи:

- Блокировать direct `Read`/`Edit`/`Write` real root в strict mode.
- Разрешать MCP server `code_sanity`.
- Добавлять context на `SessionStart`/`InstructionsLoaded`: "Use code_sanity tools for reads/search/edits".
- `PostToolUse` после edit/write вызывает sync/verify.
- Optional `FileChanged` hook для внешних изменений.

Ограничения:

- Документация показывает blocking/decision hooks, но не дает явного универсального input rewrite как у Codex.
- Поэтому Claude adapter нужно проектировать как "guard + MCP", а не как прозрачный transparent rewrite.

### 8.3 opencode adapter

Файлы:

```text
.opencode/plugins/code-sanity.ts
.opencode/package.json
opencode.json
```

Задачи:

- `tool.execute.before`:
  - если `input.tool === "read"`, заменить `output.args.filePath` на mirror path или вызвать daemon;
  - если `input.tool === "bash"`, wrap command в `code-sanity sh --sanitize-output -- <cmd>` в soft mode;
  - если edit/write tool идет в real root, перенаправить на bridge или заблокировать.

- `tool.execute.after`:
  - после edit/write вызвать sync;
  - sanitized output для compiler/test diagnostics, если API позволяет менять result.

- `file.edited` / `file.watcher.updated`:
  - background sync при внешних изменениях.

Почему opencode первый:

- API plugin явно показывает mutation `output.args`.
- Есть пример блокировки read `.env`.
- Быстрее получить end-to-end прототип.

## 9. CocoIndex integration plan

### 9.1 Где использовать

CocoIndex использовать в subsystem `indexer`:

- source: real repo через `localfs.walk_dir`;
- transforms:
  - file classification;
  - tokenizer/parser;
  - sanitizer proposal;
  - render sanitized content;
  - emit mapping records;
- targets:
  - `.code-sanity/mirror`;
  - SQLite/Postgres mapping DB;
  - optional vector/symbol index для semantic search.

### 9.2 Почему не сразу

Если текущий репозиторий Rust-only, то прямое встраивание CocoIndex добавит Python runtime, async lifecycle и dependency surface.

Для MVP можно сделать Rust-first:

- `ignore` crate для обхода repo;
- `notify` crate для watch;
- `rusqlite` для DB;
- `tree-sitter` crates для parser;
- свой incremental by hash.

Потом заменить/добавить CocoIndex как production indexer, когда понятен data model.

### 9.3 Рекомендуемый путь

Phase 1:

- Rust CLI/daemon без CocoIndex.

Phase 2:

- Python `indexer-coco/` service:
  - отвечает только за incremental mirror generation;
  - Rust bridge остается ответственным за patch mapping и agent adapters.

Phase 3:

- Опционально перенести search/symbol graph в CocoIndex/Postgres/pgvector.

## 10. Строгие режимы

### Soft mode

- Агент может читать real repo, но получает warnings.
- Hooks стараются перенаправлять obvious reads.
- Подходит для разработки и отладки.

### Guided mode

- Агенту явно добавляется context "используй code_sanity tools".
- Direct edits real repo блокируются.
- Reads через shell best-effort.
- Хороший default.

### Strict mode

- Агент запускается в sanitized mirror или overlay FS.
- Direct filesystem access to real repo недоступен.
- Edits только через bridge.
- Shell commands через `code-sanity sh`.
- Лучший режим, если цель - гарантировать, что модель не увидит raw text.

## 11. Проверки и тесты

### Unit tests

- tokenizer не ломает UTF-8 byte offsets;
- replacements не пересекаются;
- span map монотонный;
- original -> sanitized -> map roundtrip;
- patch hunks переводятся корректно;
- conflict detection на пересечении replacement spans.

### Property tests

Генерация случайных файлов:

- случайные identifiers/comments/strings;
- случайные replacements разной длины;
- случайные insert/delete/replace patches;
- проверка, что после mapping real patch + sanitize == patched sanitized mirror.

### Integration fixtures

Языки:

- Rust;
- Python;
- TypeScript/JavaScript;
- Go;
- Markdown/docs.

Сценарии:

- edit outside replacement;
- edit adjacent to replacement;
- edit inside replacement;
- rename symbol;
- formatter run;
- external real file edit;
- concurrent agent sessions.

### Agent smoke tests

- Codex:
  - hooks visible via `/hooks`;
  - MCP read returns sanitized text;
  - apply_patch through bridge changes real repo;
  - direct real edit blocked in strict.

- Claude Code:
  - settings loaded;
  - MCP tools visible;
  - direct Read real root blocked/guided;
  - bridge edit roundtrip.

- opencode:
  - plugin loaded;
  - read redirected to mirror;
  - edit triggers bridge;
  - shell output sanitized/wrapped.

## 12. Failure modes и защита

### Raw read bypass

Проблема:

- shell, IDE context, LSP, external plugin могут прочитать real repo.

Митигация:

- strict mode запускает агента в mirror/overlay;
- hooks блокируют raw filesystem MCP;
- `AGENTS.md`/session context требует sanctioned tools;
- `doctor` показывает bypassable paths.

### Drift между real и mirror

Проблема:

- real file изменился вне bridge.

Митигация:

- hash check перед каждым apply;
- file watcher;
- `sync --changed-only`;
- conflict если hunk против старого mirror.

### Патч задевает replacement

Проблема:

- агент редактирует alias, которого в real коде нет.

Митигация:

- conflict mode в MVP;
- explicit rename commands позже.

### Санитизация ломает код

Проблема:

- replacement invalid для языка или ломает public API.

Митигация:

- parser validation;
- language-specific identifier validation;
- no public API rename в MVP;
- compile/test optional after sync.

### Compiler/test output раскрывает raw names

Проблема:

- cargo/rustc/pytest вернут реальные identifiers/paths.

Митигация:

- `code-sanity sh --sanitize-output -- cargo test`;
- output sanitizer по reverse map;
- warning если hook не смог завернуть command.

### Модельный sanitizer ошибся

Проблема:

- модель заменила термин неправильно или опасно.

Митигация:

- model proposes, deterministic engine applies;
- schema validation;
- human review mode for high-impact replacements;
- audit diff `code-sanity review-sanitize`.

## 13. Roadmap

### Phase 0 - Спецификация и ограничения

Deliverables:

- этот `PLAN.md`;
- `docs/THREAT_MODEL.md`;
- `docs/HOOKS_MATRIX.md`;
- минимальный config schema.

Acceptance:

- явно описано, что hooks не являются полной read-output подменой;
- выбран первый adapter для MVP.

### Phase 1 - Rust core MVP

Deliverables:

- `code-sanity index`;
- `.code-sanity/mirror`;
- SQLite DB;
- static dictionary sanitizer;
- span map JSON;
- `code-sanity read`;
- `code-sanity verify`.

Acceptance:

- на fixture repo mirror создается детерминированно;
- повторный index без изменений ничего не переписывает;
- line endings сохраняются;
- UTF-8 offsets корректны.

### Phase 2 - Patch bridge

Deliverables:

- `code-sanity apply-patch`;
- sanitized patch -> original patch translator;
- file locks;
- patch journal;
- rollback on failure;
- conflict reports.

Acceptance:

- patches outside replacement spans применяются end-to-end;
- edits inside replacement spans conflict;
- после apply `sanitize(real) == mirror`.

### Phase 3 - opencode adapter

Deliverables:

- `.opencode/plugins/code-sanity.ts`;
- plugin install command;
- read redirect;
- edit bridge;
- sync after edit.

Acceptance:

- opencode читает sanitized mirror;
- edit меняет real repo и mirror;
- direct real edit в strict mode blocked.

### Phase 4 - MCP server

Deliverables:

- MCP tools:
  - `read_file`;
  - `search`;
  - `list_files`;
  - `apply_patch`;
  - `verify`;
- docs for Codex/Claude setup.

Acceptance:

- Codex/Claude видят MCP tools;
- read/search возвращают sanitized content;
- apply_patch меняет real repo.

### Phase 5 - Codex hooks

Deliverables:

- `.codex/hooks.json`;
- hook scripts;
- `install-hooks --agent codex`;
- `doctor --agent codex`.

Acceptance:

- hooks проходят trust review;
- direct `apply_patch` real root blocked in strict;
- MCP code_sanity allowed;
- best-effort shell read предупреждает или переписывается.

### Phase 6 - Claude Code hooks

Deliverables:

- `.claude/settings.json`;
- hook scripts;
- MCP guard config;
- `doctor --agent claude`.

Acceptance:

- Claude использует MCP tools;
- direct raw edit blocked;
- post-edit sync работает.

### Phase 7 - Model-based sanitizer

Deliverables:

- proposal schema;
- local/offline provider interface;
- review queue;
- deterministic alias registry;
- allowlist/denylist policy.

Acceptance:

- модель не пишет напрямую в mirror;
- все proposals проходят validation;
- audit report показывает каждую замену.

### Phase 8 - CocoIndex indexer

Deliverables:

- Python `indexer-coco/`;
- localfs source;
- mirror target;
- DB target;
- live mode;
- optional Postgres/pgvector search.

Acceptance:

- incremental reindex пересчитывает только changed files;
- mirror target synchronized;
- Rust bridge продолжает владеть patch mapping.

### Phase 9 - Strict mode

Deliverables:

- sanitized worktree runner;
- optional FUSE/overlay prototype;
- shell wrapper;
- raw output sanitizer.

Acceptance:

- агент физически не видит real root;
- tests/builds запускаются через controlled bridge;
- known bypass list documented.

## 14. Пример пользовательского flow

```bash
code-sanity init
code-sanity index
code-sanity install-hooks --agent opencode
code-sanity serve
opencode .
```

Агент:

1. Просит файл `src/main.rs`.
2. Adapter возвращает `.code-sanity/mirror/src/main.rs`.
3. Агент делает edit.
4. Adapter отправляет patch в `code-sanity apply-patch`.
5. Bridge применяет patch к real `src/main.rs`.
6. Bridge обновляет mirror.
7. Agent видит sanitized результат.

## 15. Рекомендуемый первый MVP для этого репозитория

Так как текущий репозиторий пустой Rust project, лучше начать не с CocoIndex, а с маленького Rust core:

1. `src/config.rs` - config и path layout.
2. `src/index.rs` - обход файлов, hash, mirror generation.
3. `src/sanitize.rs` - deterministic replacements.
4. `src/map.rs` - span map.
5. `src/patch.rs` - patch parser и translator.
6. `src/cli.rs` - команды.
7. `fixtures/basic-rust/` - тестовый repo.
8. `tests/roundtrip.rs` - end-to-end.

Параллельно держать CocoIndex как Phase 8, иначе ранний MVP утонет в интеграции Python/Rust вместо проверки главного риска: patch back-projection.

## 16. Что считать успехом

MVP successful, если выполняется:

```text
real file
  -> index
sanitized mirror
  -> agent patch
original patch
  -> apply to real
resanitize changed file
  -> same sanitized state as if patch was applied directly to mirror
```

То есть главный инвариант:

```text
sanitize(apply_original_patch(original)) == apply_sanitized_patch(sanitize(original))
```

Для всех patches вне replacement spans. Все остальное можно наращивать после этого.

## 17. Итоговая оценка резонности

Резонно:

- как privacy/lexical-normalization layer;
- как защита от ложного framing по токсичным или доменным словам;
- как агентный workflow через sanitized mirror + bridge;
- как incremental indexing задача.

Не резонно:

- пытаться делать это только хуками;
- обещать 100% read interception без sandbox/overlay;
- позволять модели редактировать sanitized aliases без явной политики;
- начинать с model sanitizer до готового deterministic patch bridge.

Лучший порядок:

1. Span map + mirror.
2. Patch bridge.
3. Один adapter, лучше opencode.
4. MCP server.
5. Codex/Claude hooks.
6. Model proposals.
7. CocoIndex incremental production path.
