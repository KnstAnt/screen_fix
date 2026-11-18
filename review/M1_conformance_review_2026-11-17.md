# Статическая проверка соответствия кода M1 документации

Дата: 2026-11-17. Ветка `M1`, рабочее дерево с незакоммиченными изменениями.

## 0. Как проводилась проверка

- **Метод.** Только чтение кода. Сборка, `cargo check`, тесты, форматтер и приложение не запускались: `doc/SCREEN_FIX_PROJECT_CONTEXT.md` запрещает это без отдельного разрешения. Ни одна находка не подтверждена исполнением.
- **Документы.**
  - `Intercall Recorder technical spec v1 Windows.pdf`: извлечённый текст, разделы 1–7 и 10–17.
  - `SCREEN_FIX_M1_SPEC_ADDENDUM.md`, `SCREEN_FIX_M1_ARCHITECTURE_ADDENDUM.md`, `SCREEN_FIX_M1_ALGORITHMS_ADDENDUM.md`.
  - `SCREEN_FIX_ELEVATED_COMPONENT_SPEC_ADDENDUM.md`, `SCREEN_FIX_SPEC.md`, `SCREEN_FIX_ARCHITECTURE.md`, `SCREEN_FIX_PROJECT_CONTEXT.md`.
  - `README.md`, `deploy/windows/*`.
- **Охват.** Весь Rust-код workspace, около 41,6 тыс. строк в 8 подсистемах: сессия/UI, видео/захват, stills/пакет, ввод Recorder, helper/native-observation, контекст/UIA, elevated collector, elevated selector на стороне Recorder.
- **Уровни.**
  - **Critical**: теряются или искажаются основные данные при обычной работе.
  - **High**: нарушен MUST, и это проявляется на штатном сценарии.
  - **Medium**: нарушение проявляется на частом, но не основном пути, или нарушен архитектурный контракт.
  - **Low**: редкий путь, неточные метаданные, мёртвый код.
- **Пометки.**
  - ✔ — находку я перепроверил по коду лично.
  - «предп.» — путь в коде подтверждён, но частоту или величину эффекта без запуска не оценить.

---

## 1. Главное (Critical / High)

### 1.1 Ввод

| # | Уровень | Проблема | Код | Требование |
|---|---|---|---|---|
| I-1 ✔ | **Critical** | Любые две записи `Motion` считаются совместимыми. Окно сопоставления ±20 мс, и `motion()` требует, чтобы у каждого raw-кандидата был ровно один hook-кандидат. На 125–1000 Гц в окне всегда несколько hook-событий, поэтому результат `Ambiguous`, а `approved_receipt` возвращает `None` и запись подавляется без строки и счётчика. Ambiguous ещё и распространяется на соседей. Колесо с одинаковым `value` ведёт себя так же. Итог: `pointer_move` при перетаскивании и быстрый скролл почти полностью теряются. | `core/src/capture/input/matching.rs:14,558-579,636`; `core/src/platform/windows/input/gate.rs:981-985` | C3 «Ambiguous … remain unknown with a reason»; ALG §6.1 «unpaired observations»; PDF INP-6 «Keep all samples while a button is held»; T7 |
| I-2 | High | Все корреляции, кроме `Matched`/`Unmatched` (`Ambiguous`, `MissingScan`, `Discontinuous`), удаляются молча. На закрытии `finish()` всё, что моложе 40–60 мс от cutoff, получает `Discontinuous`. Поэтому теряются реальные переходы перед каждой Pause/Stop и около разрывов. Отброшенный `up` даёт ложный `repeat:true` на следующем нажатии. | `gate.rs:983`; `matching.rs:371-386,485-490`; `receiver.rs:455-468` | C3; ALG §6.1 п.3–4; INP-6 «Keep every button and key transition» |
| I-3 ✔ | High | Injected-ввод фактически не публикуется. Injected hook (`flags & 0x12`) помечается `unqualified` и никогда не сопоставляется. Approval берётся только от raw-слота, поэтому hook-only injected подавляется. `injector_hints` вычисляются, но не сериализуются. | `matching.rs:627,641-647`; `gate.rs:988-990`; `evidence.rs:180-190` | PDF INP-7; ALG §6.1 «Unmatched injected hooks and VK_PACKET can be legitimate input» |
| I-4 | High | Записи разрывов не содержат count и first/last. Счётчики `private_gaps`, `suppressed`, `dropped` пишутся, но не читаются. Полноценная `gap` есть только при переполнении очереди журнала. | `receiver.rs:164-178,776-779,949-965`; `gate.rs:181-198,893-903` | PDF REL-2; ALG §6.4; ARCH §5 «offered, retained, suppressed, dropped and persisted» |
| I-5 | High | Если клавишу держали через паузу или разрыв, первый автоповтор после resume публикуется как реальный `key_down repeat:false` и запускает pre-action still. Обратный случай: `key_up` без опубликованного `down` удаляется. Для кнопок мыши это сделано правильно (`press_unavailable`). | `held.rs:287-291`; `gate.rs:526-531,572` | ALG §4 «not given a fabricated physical down»; C5 |

### 1.2 Видео и сессия

| # | Уровень | Проблема | Код | Требование |
|---|---|---|---|---|
| V-1 ✔ | High | `MAX_SEGMENT_PICTURES = 61*30+2` рассчитан на 30 FPS, а сегмент длится 60 с. На выбираемом в UI профиле «60 FPS pilot» примерно через 30 с `stage_slot` возвращает ошибку, и видео монитора выключается до конца попытки. Кольцо `EVIDENCE_SLOTS=2048` тоже меньше сегмента на 60 FPS. | `core/src/platform/windows/video.rs:29-30,644-648`; `slots.rs:92`; `ui/src/app.rs:322` | PDF VID-11 (60 fps), VID-7 (~60 с) |
| V-2 | High | Кадр с `source` в слоте k, полученный после конца k, не попадает ни в k, ни в k+1. На статичном экране после одиночного изменения видео показывает старый кадр до следующего WGC-кадра (предп.: примерно половина одиночных изменений на 30 fps). | `video.rs:180-186,417-420`; `monitor.rs:430-433` | PDF VID-4; ALG §9.1 |
| V-3 | High | Полное декодирование сегмента (`verify_decoded_frames`, программный NV12) идёт синхронно на видео-воркере внутри `output()`. На каждой ротации очередь захвата (`sync_channel(2)`) переполняется и слоты получают `QueuePressure`. При suspend не укладывается в 2 с (см. S-1). У асинхронного MFT срабатывает ложный watchdog 5 с. | `video.rs:562`; `mp4.rs:96-102,144-199`; `encode.rs:99-102` | ALG §9.2 «bounded handoff»; CLK-7; §15 |
| S-1 ✔ | High | Обычный сон переводит всю запись в `Failed`. `segment_deadline` = suspend + 2 с и `pause_deadline` 10 с считаются по `Instant` (QPC идёт во сне). Если видео не закрыло сегмент, возвращается `Err("Video did not finish within the suspend budget")`. | `core/src/session/windows.rs:1228-1233,1284-1289` | A2 «Normal operation remains required: … lock/sleep boundaries»; CLK-7 «stays .part» |
| P-1 ✔ | High | Одна gap-строка в `frames.jsonl` (учтённая потеря очереди) приводит к `InconsistentInventory`, а затем к `capture=Failed`. Корректно финализированный пакет с записанной потерей объявляется проваленным. | `core/src/package/metadata.rs:563,733-736,830-831` | C6 «may still contain explicitly recorded losses»; REL-2 «Overflow never ends the attempt» |

### 1.3 Контекст, UIA, приватность

| # | Уровень | Проблема | Код | Требование |
|---|---|---|---|---|
| C-1 ✔ | High | `editable()` расходится с правилом CTX-9 в обе стороны. (а) Если нет Value/Text pattern и тип контрола не из списка, возвращается `None`. Тогда `PossiblySensitive`, и все клавиши визита сворачиваются в один `redacted`, включая Ctrl+S в Blender viewport (Pane). Это противоречит примеру PDF `{"control":"Pane","editable":false}` и T11. (б) Для Button/Menu/ListItem/… возвращается `Some(false)` вместо `null`. (в) При `ValueIsReadOnly=true` Text pattern не проверяется. | `core/capture/native-observation/src/uia.rs:510-541,268-273`; `gate.rs:515-524` | PDF CTX-9, INP-4, T11; ALG §7.1 |

### 1.4 Elevated

| # | Уровень | Проблема | Код | Требование |
|---|---|---|---|---|
| E-1 ✔ | High | Смена окна или фокуса во время Preparing/Armed/Activating роняет всю попытку. `maintain()` на heartbeat 250 мс возвращает `Err(TargetChanged)`, `fail_source` даёт FailedPrefix. Исход зависит от гонки с `Invalidated` от collector. | `core/src/platform/windows/elevated/visit.rs:181-187,199-212,515-518`; `selector.rs:154-176` | ELEVATED §4.2 «closes the associated grant and waits for a fresh valid foreground»; C5 |
| E-2 ✔ | High | Пока классификация foreground `None`/`Unknown` (после каждой смены HWND до прихода результата), ввод стандартного helper (Input/Progress/Gap) выбрасывается без gap-записи и без interrupt Receiver. Пример: отпускание Alt в Alt+Tab, после чего held-state рассинхронизирован. | `selector.rs:359-366,937-939`; `input.rs:253-265`; `receiver.rs:200-211` | ALG §4 п.5 (discard только для *inactive*); C3 «bounded loss/resync» |
| E-3 ✔ | High | В collector истечение дедлайна UIA/IME-провайдера фатально: supervisor вызывает `lifetime.stop()`, `cancel()`, затем `TerminateProcess`. Запаса между дедлайном слота и собственными таймаутами провайдера (500 мс UIA, `SendMessageTimeoutW`) нет. Медленный, но рабочий провайдер роняет запись. | `core/capture/elevated/src/collector.rs:127`; `runtime/lifetime.rs:120-131`; `runtime/context.rs:336-343,403,523-527` | ELEVATED §5.2 «provider timeout … via current typed unavailability» |

### 1.5 Верификация

| # | Уровень | Проблема | Код | Требование |
|---|---|---|---|---|
| T-1 ✔ | High | В workspace нет ни одного `#[test]` и каталога `tests/`. `PackageReader::validate_v5_artifacts` нигде не вызывается. | `core/src/package/reader.rs:132` | M1-10 «reproducible checks for journal ordering/parsing, frame/segment mapping, input boundary consistency and pause-stream rules» |
| T-2 ✔ | High | Тестового продюсера confirmed-save нет. `confirm_fixture_save` / `route_fixture` не вызываются ни из UI, ни из тестов, поэтому триггеры checkpoint/final-save-settled недостижимы и T22 для них проверить нечем. | `core/src/session.rs:388`; `core/src/platform/windows/input.rs:1223` | M1-08 «use a test producer until M2» |

---

## 2. Сквозные темы

### 2.1 Молчаливые потери (без gap-записи, счётчика или причины)

Это самая массовая проблема. Она нарушает REL-2, ALG §6.4, ARCH §5 «Each admitted request has one terminal outcome», C6.

**Ввод**

- До успешной opening-resync после старта, resume или разрыва все пакеты выбрасываются; ожидание не ограничено (`receiver.rs:138-140,333-347,594-599`).
- Raw-записи при недоступном foreground с более высоким уровнем целостности (higher-integrity) выбрасываются до matcher. `PhysicalHeld` не видит отпусканий (`receiver.rs:128-130`).
- `ControlDisposition::Uncertain` удаляется как Recorder-ввод (`control.rs:184-234`; `gate.rs:343-345`).
- Реальное устройство, которого ещё нет в инвентаре, превращается в NULL-handle: SM_SWAPBUTTON не применяется, origin становится unknown (`devices.rs:66-90`).

**Контекст**

- TextEdit-коммиты: переполнение 4 слотов (`take_loss` не вызывается в обычном пути), коммиты длиннее 1024 UTF-16 и невалидный UTF-16 теряются молча (`native-observation/src/uia_commits.rs:69,168,177`; `context.rs:710-714`).
- Очереди clipboard, `cursor_completions` и cursor assets (`clipboard.rs:127,139`; `context.rs:538-540,613-615,790`; `cursor.rs:121-127`).
- `context_notifications_gap` записывается только как счётчик, без интервала (`context.rs:337`).
- WinEvent-callback не фильтрует по `GA_ROOT`, поэтому события дочерних окон вытесняют события top-level окон из кольца на 512 (`events.rs:282-298`).

**Stills**

- PNG Inbox отбрасывает задание при `try_lock()`-конкуренции и пишет причину `Capacity` (`still_png.rs:102-105`).
- Неверные причины в drop-строках (`stills.rs:542-543,559-560,914`).
- Ошибки записи PNG не становятся health-записью, а UI показывает «Recording complete» (`still_png.rs:262-305`; `platform/windows.rs:305`).

**Видео**

- Если timeline недоступен (гонка при Resume), кадр не передаётся в видео без следа (`monitor.rs:351-352,603-613`).

**Elevated**

- При смене scope serial `Losses::default()` стирает накопленные loss-диапазоны (`native-observation/src/input/raw_loop.rs:222-228`).
- Gap, пересекающий точку resync, отбрасывается целиком (`ingress.rs:83`, предп.).
- Отклонённые text-edit commit в collector (`runtime/context.rs:583`).

**Text**

- `text_commit status:"unknown"` не пишется никогда; поздние TextEdit-коммиты отбрасываются (`text.rs:42-47,130-156`; `gate.rs:322-337`). Это нарушает PDF INP-3.

### 2.2 Транзиентные или некритичные ошибки завершают всю попытку

Нарушаются A2 (не путать с «никогда не скрывать сбой»), VID-9 «none ends the attempt», SPEC §1 «explicit suppression … outcomes».

| Где | Что |
|---|---|
| `display.rs:118,162-176`; `monitor.rs:222`; `video.rs:333-351` | Ошибка `display::enumerate()` («topology changed during discovery», SDR white/DPI у *любого*, даже невыбранного монитора) приводит к `StopOnError` |
| `context/identity.rs:135-149`; `context.rs:313` | Непроверяемый путь процесса с тем же exe-именем или canonicalize дольше 2 с роняет `Context::poll` |
| `selector.rs:106-115`; `preflight.rs:217-227`; `session/windows.rs:589-609` | Одобренный foreground с неподдерживаемым токеном (DifferentUser, ProtectedProcess…) во время записи вызывает `fail_classified_source` вместо подавления |
| `native-observation/src/input/raw.rs:74-78,103-149,243` ✔ | Touchpad `(0x0D,5)` и 3D-мышь `(1,8)` (SHOULD) регистрируются всегда; их расхождение — фатальная ошибка helper. Кроме того, они делят `Producer::RawInput` с клавиатурой и мышью, так что их переполнение даёт общий gap. Нарушает M1-05 «do not silently promote…» |
| `session/windows.rs:1476-1482,1523-1529` ✔ | Если отметка прогресса хоткеев старше 250 мс (например, после сна), все хоткеи, включая Stop, отключаются до конца записи. Нажатия молча выбрасываются |
| `receiver.rs:27,514-521` | Остановка прогресса producer больше 250 мс убивает helper до конца записи |
| `elevated/src/runtime/owner.rs:30,161,193` | Запись в data-lane дольше 100 мс разрывает всю привязку, хотя должна давать явную потерю (ELEVATED §3.2) |
| `native-observation/src/input/raw.rs:411-436`; `capture/input/src/windows.rs:138-211` | Один некорректный Raw Input-пакет или `PacketError` убивает helper, причём со стадией `MessageLoop` / `transport_error` |
| `encoder.rs:66-78`; `video.rs:351-356` | Обратная сторона: при ресайзе HW-энкодер может молча смениться на SW 15 fps (это runtime fallback, запрещённый A2). Причина отбрасывается |

### 2.3 Схемы журналов расходятся с PDF, версионированного описания расширений нет

ALG §3.2: «exact serialized field names follow the PDF; any metadata extensions are versioned and documented».

- **`input.jsonl`**
  - `mods` в коде — битовая маска `modifiers`/`modifiers_known`; `scan` — объект.
  - У pointer-строк поля `pos/pos_recv/monitor/held/delta/coalesced` вложены в `pointer{…}`, `held` означает «до», а не «после».
  - Записи `input_health` под этим именем нет; blind пишется в `context.jsonl` и только на Stop, с началом в момент обнаружения (`gate.rs:567-703,1033-1051`; `input.rs:1101-1115`).
- **`context.jsonl`**: `foreground_window` вместо `foreground`, `pointer_element` вместо `element`, `process_id/executable/bounds` вместо `pid/exe/rect`, числовой `control_type`, `"timed_out"` вместо `"timeout"`, отдельная строка `ime_state` (`context.rs:422-434,880-888`; `queries.rs:457-493`).
- **`stills.jsonl`**: before и seen — отдельные строки; URI `raw/images/…`, а не `images/…`; `frame_obs_id` неоднозначен между capture-эпохами (`stills.rs:274-300`; `package.rs:96`).
- **`frames.jsonl`**: `recv_t_us` — время публикации строки (до ~60 с после захвата), а не получения кадра (CLK-2) (`slots.rs:162-201`; `video.rs:53-55`).
- **Манифест**: нет сводки `health` (pauses/gaps/blind/dropped_stills) и инвентаря `raw/images`/`raw/cursors` (`metadata.rs:478-509,853-860`).

README частично описывает фактический формат, но поправки M1 к схемам PDF нет.

### 2.4 Нарушения архитектурных правил

| Правило | Нарушение |
|---|---|
| ARCH §4 «no shared mutable session object queried by every worker» | `Arc<Mutex<Facts>>`/`WorkerSession` клонируется во все потоки, и они его *меняют*: `video_failed`, `input_failed`, `set_source_failure`, `disk_sample`… (`session/windows.rs:167-227,344-350`) |
| C5 «one explicit recording state machine» | Состояние размазано: `SessionPhase` без Idle, `stop` — отдельный `AtomicBool`, плюс `source_change`, `clock_failed`, `complete_publication` и локальные флаги `coordinate` |
| VID-12 / ARCH §5 «All immediate-context operations stay [on capture thread]» | `encoder::self_check` (`VideoProcessorBlt`, `CopyResource`/`Map`) выполняется на видео-потоке на устройстве захвата (`video.rs:342-351`; `encoder.rs:161-183`). Гонку смягчает `SetMultithreadProtected` |
| PDF ARCH-2 (callbacks: без lock/RefCell, с `catch_unwind`; input-потоки только читают) | `thread_local RefCell` в hook (`native-observation/src/input/hooks.rs:142-144,251-268`); `Mutex` в power-callback и WinEvent без `catch_unwind` (`session/windows/environment.rs:274-298`); блокирующие `CreateFileW`/`HidD_*`/`SetupDi*` на Raw Input-потоке (`input/device.rs:59-206`); `OpenProcess`/`GetProcessTimes`/`GetGUIThreadInfo` на каждый WM_INPUT (`raw.rs:166,287-353`); в collector `verify_capture_target` (токены) на каждый WM_INPUT и каждую исходящую запись (`elevated/src/runtime/input.rs:50`; `grant.rs:111,164-180`) |
| PDF INP-1 «GetRawInputBuffer until it returns 0» | В governed (collector) пути buffered drain отключён (`raw_loop.rs:246`) |
| PROJECT_CONTEXT «Избегай глобального изменяемого состояния» | `static RECORDING_OWNED: AtomicBool` (`session/windows.rs:53`), `REPORT_SERIAL` (`diagnostics.rs:165`), `thread_local DESKTOP_SIGNALS` с сырым указателем (`environment.rs:61-63`). Назначение понятно, но исключение не задокументировано |
| C2 «shared as immutable image data rather than copied for each reason» | Readback, CPU-буфер и PNG-задание создаются на каждую логическую ссылку (`stills.rs:1108-1133`). Staging-текстура пересоздаётся на каждую задачу (`readback.rs:211-221,268`) |

---

## 3. Остальные находки по подсистемам (Medium / Low)

### Сессия, UI, диагностика

- **M.** Если Stop-дедлайн истекает до авторизации, первая причина отказа (`CaptureError` воркера) теряется; остаётся только «could not finish before its deadline» (`session/windows.rs:1120-1127`). C6, M1-09.
- **M.** Для не-elevated отказов после admission UI не отличает их от preflight-ошибок и не показывает опубликованный Failed-пакет; `close_after_recording` теряется (`platform/windows.rs:279-281`; `ui/src/app.rs:154-212`). M1-01.
- **M.** Нет причины паузы `non_approved_app` / `always_pause_window` и записи `late_pause`; неодобренный foreground только подавляется (`session.rs:41-48`). PDF CTX-6/PRIV-2. См. раздел 5, п. 3.
- **L-M.** Любая пауза в elevated-эпохе (User, Lock) помечается `ElevationTransition` (`session/windows.rs:1263-1269,1651-1658,1726-1731`).
- **L-M.** `CaptureError::backend` всегда ставит `FailureComponent::WindowsCapture`, а `Controller` в core не используется (`error.rs:59-63`). M1-09 «affected component».
- **L.** Lock/secure desktop во время отсчёта: отсчёт не перезапускается, причина в UI не видна (`session/windows.rs:1315-1408`; `app.rs:440-458`).
- **L.** `EffectiveCapabilities` и калибровки — заглушка, всегда `Unavailable(NotObserved)` (`config.rs:136-146`; `platform/mod.rs:89-95`).
- **L.** Если сессия завершилась ошибкой, диагностика индикатора теряется (`app.rs:157-186`).
- **L.** `check_deadline()` после успешного `MoveFileExW` может объявить уже заменённый манифест «не подтверждённым» (`metadata.rs:743-751`). Аналогично уже опубликованный `.mp4` переименовывается обратно в `.part` (`mp4.rs:111-119`).
- **L.** После отказа авторизации остаётся `manifest.pending.json`, и повторный publish упадёт на `create_new` (`metadata.rs:757-776`).

### Видео

- **M.** Смена DPI/позиции сбрасывает источник повтора; слоты становятся `miss(BeforeFirstFrame)` вместо repeated (`monitor.rs:334-350`; `video.rs:458-497`). VID-9.
- **L.** HDR без уровня SDR white даёт отказ вместо деградации в `system_bgra8` (`monitor.rs:628-630`). VID-8.
- **L.** Метаданные битрейта: заданные значения записаны под «измерительными» именами (`video.rs:575-576`).
- **L.** Причины отказа HW-кандидатов теряются (`encoder.rs:67-75`).
- **L.** SW-путь делает полный NV12 readback каждого кадра на 30 fps, хотя кодирует 15 fps (`gpu.rs:190-219`).

### Stills и пакет

- **M.** Scroll без атрибуции устройства: каждый тик становится отдельной вспышкой с pre_action. Before-кадры освобождены от лимита 6/с, поэтому PNG-нагрузка растёт без ограничения (`stills.rs:605,856-871`; `still_png.rs:255`).
- **M (предп.).** Motion-конверты занимают бюджет `EarlyRows` (1024) при удержанном input-префиксе (`receiver.rs:921`; `still_rows.rs:20`).
- **M (предп.).** Приватные provisional-кандидаты держат 2 из 3 hold и не участвуют в вытеснении по приоритету (`native.rs:15`; `stills.rs:471-474,749-780`).
- **L.** Trigger `start` не проверяет `last_evicted()` и может пометить более поздний кадр (`stills.rs:518-529`).
- **L.** Для elevated-визитов калибровка отбрасывается, но помечается как `missing_calibration` (`stills.rs:464,827`).
- **L.** `expire_drain` при Stop может оставить осиротевшие `raw/images/*.png` (`stills.rs:1046-1052`).
- **L.** Reader проверяет `event_ref` только по диапазону seq (`reader.rs:232-240`).
- **L.** Health-строка журнала может получить `"recv_t_us": null` (`journal.rs:577-589`).

### Контекст

- **M.** Ghost-окно зависшего одобренного приложения не считается одобренным (`context.rs:344,396,867`). PDF IMG-8/PRIV-2.
- **M.** Ограничитель LOCATIONCHANGE выпускает самое старое событие пачки без trailing-перечитки, поэтому конечная геометрия после перетаскивания не попадает в журнал, а `visit_geometry` устаревает (`context.rs:430,499-514`). CTX-1/CTX-2.
- **M.** Зависший или не поднявшийся UIA-worker не диагностируется; все запросы молча получают `Unavailable`/`TimedOut` (`context/uia.rs:146,162-166`). ALG §7.1, A2.
- **L.** `ES_PASSWORD` (0x20) проверяется у окна любого класса, хотя у других классов этот бит означает другое (`native-observation/src/windows.rs:97`).
- **L.** UIA-ошибка UIA_E_TIMEOUT пишется как `unavailable`, а не `timeout` (`uia.rs:233-235`).
- **L.** Без FocusSnapshot пишется `ime_state: not_ime` вместо `unknown` (`context/ime.rs:62-64`).
- **L.** Нет fallback на `hwndActive` при NULL `hwndFocus` (`native-observation/src/windows.rs:46`). INP-2.
- **L.** Подписанные MINIMIZE*/SWITCH*/IME_* не дают записей (`events.rs`; `context.rs:373-375`). CTX-1.
- **L.** Клик во время незавершённого pointer-запроса остаётся без чтения (`queries.rs:220-222`). CTX-10.
- **L.** Clipboard больше 64 KiB пропускается без записи (`clipboard.rs:244,293-295`).
- **L.** CTX-12: нет версии и режима драйвера планшета (`settings.rs:83-121`).
- **L.** Одобренный foreground при неудачном `WindowMetadata::read` пишется как `approved:false` (`context.rs:434-435`).

### Ввод и helper (дополнительно)

- **L.** `LowLevelHooksTimeout` читается только как `REG_SZ` (`native-observation/src/input.rs:245-261`). INP-12.
- **L.** Беззнаковые HID-поля зависят от статуса `HidP_GetScaledUsageValue` (`hid.rs:306-322`). INP-11.
- **L.** Progress hook-продюсера формирует цикл потока, а не сам hook, поэтому coverage подтверждается и после снятия hook Windows (`hooks.rs:169-177`).
- **L.** В UIA 4 cache request и живые вызовы; дедлайн проверяется только между вызовами (`uia.rs:179-182,247-525`). CTX-9 «one cache request».
- **L.** `transmute` недокументированных `HungWindowFromGhostWindow`/`GhostWindowFromHungWindow` без инварианта сигнатуры (`hung.rs:33-46`).
- **L.** `Drop` для `InputHelper` ждёт с `INFINITE` (`input_helper.rs:260-266`). A2 «bounded cleanup».
- **L.** Первый HID Scan Time получает `source_t_us = recv_t_us` без uncertainty (`input/hid.rs:249-253`). C1.
- **L.** Абсолютные пакеты мыши не сохраняют сырые x/y (0..65535) и флаги (`evidence.rs:93`; `gate.rs:699-701`). INP-5.
- **L.** `&mut` над межпроцессной разделяемой памятью (`protocol/src/ipc/shared.rs:183,200`).
- **L.** `pen_pressure`: добавлено `disabled`, нет `unavailable_wintab` (`metadata.rs:385-397`).

### Elevated (дополнительно)

- **M.** Collector не связывает approval-ключ с реальным образом цели: проверяется только, что ключ есть в списке и что `claim.image` равен пути процесса (`elevated/src/runtime/grant.rs:80,188`; `witness.rs:486-488`). ELEVATED §4.1 «Collector authorization requires both … independent proof». В документе это место неоднозначно.
- **M.** Отложенные elevated checkpoint/fixture при Pause/Stop закрываются без итоговой диспозиции, включая уже сертифицированные (`input.rs:430-447,1169-1220`). ELEVATED §5.3, M1-08.
- **M.** После admitted-отказа host дропается без `Control::Stop` и terminal ack, а `stopped=false` и `cleanup_failure` не заполняются (`selector.rs:376-394`). C6.
- **M (предп.).** После запроса Stop lease не продлевается до `Control::Stop`, поэтому при обработке дольше 1 с обычный Stop превращается в `LeaseExpired` (`selector.rs:102`; `peer.rs:50-56`).
- **L.** Готовность Armed `held: Known/Unknown` принимается и отбрасывается, в evidence не попадает (`visit.rs:309`; `metadata.rs:367-374`).
- **L.** Setup/stop-наблюдения неполные: нет dispatch/return UAC, отозванного запуска, исхода held-пробы (`metadata.rs:367-374`). M1-09.
- **L (предп.).** При возврате на стандартный источник регистрация helper и hook-health не перепроверяются (`selector.rs:486-494`).
- **L.** Bootstrap-пайп выдаёт `FILE_ALL_ACCESS` logon SID'у (`elevated/src/pipe/security.rs:401`).
- **L.** Arm QPC берётся в `prepare`, до регистрации хуков (`grant.rs:92`).
- **L (предп.).** Секрет `Secret<32>` перемещается по значению, и стековые копии не стираются (`wire.rs:159-191`).
- **L.** `ticks()` неявно рассчитан на `LEASE == 1 s` (`lifetime.rs:230,290-294`).

---

## 4. Мёртвый код и остатки

| Где | Что |
|---|---|
| `core/src/capture/observation.rs:10-21,50-128` | `ObservationTrigger`, `ObservationRequest`, `ObservationSelection` не используются |
| `core/src/capture/frame.rs:90-106` | `SegmentPts100ns`, `VideoSlot`, `SlotDisposition` не используются |
| `core/src/capture/context.rs:11-50` | `ForegroundObservation`, `ApprovalStatus`, `FocusClassification`, `MAX_WINDOW_TITLE_BYTES` не используются |
| `core/capture/input/src/lib.rs:6-37` | `InputAttachment`, `InputControl` не используются |
| `protocol/src/ipc.rs` | `CoreToHelper::EnvironmentChanged/Stop` не отправляются: Stop и environment идут через атомики `shared.rs` |
| `monitor.rs:120-129` | Недостижимая ветка `video == None` |
| `core/src/platform/windows/context/{uia_commits,windows}.rs` | Однострочные реэкспорты (живые). Модуль `windows` затеняет крейт `windows`, отсюда `::windows::…` |
| `README.md` | Пишет, что `Cargo.lock` «absent», но файл отслеживается git ✔ |

---

## 5. Противоречия и пробелы в самой документации

Их стоит разрешить до правок кода: часть находок выше прямо зависит от выбранного толкования.

1. **Восстановление регистрации и хуков.** PDF INP-1 (re-register + `input_health`) и INP-12 (watchdog переустанавливает hook) против ALG §6.1 «not implemented». A2 явно откладывает только перезапуск helper, а §1 разрешает отменять PDF лишь явными поправками.
2. **Роль helper.** PDF §2/INP-9: helper выполняет matching, blind и отбрасывает control-hotkey. ARCH §4/§5 переносят это в Recorder. Явной поправки к INP-9 нет.
3. **Неодобренный foreground.** PDF CTX-6/PRIV-2: пауза `non_approved_app` + `late_pause`. ALG §4 п.5, SPEC §1, ELEVATED §4.2: «stays suppressed». Поправки A-уровня нет.
4. **Отказ энкодера и suspend.** VID-6 «never end the attempt» и REL-6 (recreate) против A2 «Stop the affected activity». CLK-7/REL-9 «stays .part for REL-3», но REL-3 отложен; что M1 делает при превышении 2 с, не определено (к S-1).
5. **Сопоставление движения.** INP-6 (сэмплировать только matched) и ALG §6.1 (±20 мс, однозначность, «unpaired observations») при плотном движении несовместимы, и не сказано, что публиковать (к I-1).
6. **Injected-ввод.** INP-4 требует foreground на приёме, а hook его не несёт. INP-7 требует origin по флагам hook, а injected hook не сопоставляется. Путь approval для hook-only не описан (к I-3).
7. **Editable.** CTX-9 не говорит, что делать при полном отсутствии Value/Text, а пример Blender Pane даёт `false`. PDF INP-4 (`editable:null` означает `typing:"unknown"`, клавиши сохраняются) против ALG §7.1 (aggregate redacted visit) (к C-1).
8. **Схемы журналов.** ALG §3.2 требует точные имена полей PDF, но PDF для `context.jsonl` и частично `input.jsonl` даёт только примеры строк. Нормативность примеров не определена (к 2.3).
9. **Stills-строка и URI.** PDF §5 (одна строка с `file`/`seen_file`, `images/…`) против README (`raw/images/…`, раздельные строки).
10. **`checksums.sha256` (DATA-3, MUST)** не упомянут ни в одном M1-аддендуме, но §7 аддендума говорит «No scope … removed from the PDF». Неясно, обязателен ли он в M1; в коде его нет.
11. **Manifest `health`** (PDF §12) против «minimal manifest» (M1-09): какие поля обязательны в v5.
12. **Complete.** SPEC §3 «usable video for each selected monitor» против A2 (удаление монитора — нормальная работа) и REL-7.
13. **Вытеснение.** ALG §8.5 «Equal priority uses oldest first» против REL-2 «drop the newest».
14. **PNG worker.** PDF IMG-3 (пул до 4 потоков, 256 jobs / 256 MB на монитор) против C2 «one PNG worker» без числовой границы.
15. **Хоткеи.** PDF INP-9 задаёт по умолчанию Ctrl+Alt+F12 для Stop, а Windows резервирует F12 под отладчик (`RegisterHotKey`). Код только предупреждает об этом ✔. Поведение при потере хоткеев во время записи не определено.
16. **Поток контроллера.** ALG §2 «Session control can share the UI event loop» против ARCH §5 «has its own thread».
17. **WGC-время.** ALG §3.1 «subtract before scaling» против формулы PDF CLK-3; код следует PDF.
18. **`recv_t_us` для строк кадров.** CLK-2 против ALG §3.2 «Frame rows wait for the corresponding encoder outcome».
19. **Elevated.**
    - Неподдерживаемый approved foreground после admission: ELEVATED §1 «closes admission **or** is suppressed».
    - Потеря армированной цели: §4.1 «blocks startup or resume» против §4.2 «waits for a fresh valid foreground».
    - Не определено, получает ли каждая повторная подготовка гранта новый бюджет readiness 5 с.
    - Что фиксировать, если terminal ack невозможен.
    - «No … JSON rows … cross this wire» при `serde_json`-конверте.
    - Pending grant (§3.2) против последовательной схемы в §4.2.
    - Где делается canonical approval match (к находке про approval-ключ).
20. **CTX-4.** Hidden-window и перечитывание мониторов на одном «context thread» против ARCH §5 (разные циклы).
21. **«consent» в M1-01**: UAC-согласие collector'а или consent box PDF §14 (M4)?

---

## 6. Требования, не реализованные совсем или почти

- M1-10: воспроизводимые проверки (T1 gates 1–4), тесты, вызов `validate_v5_artifacts`.
- M1-08: тестовый продюсер confirmed save/checkpoint.
- REL-2: gap-записи с count и first/last для потерь helper/Receiver; отчёт о счётчиках suppressed/dropped.
- INP-3: `text_commit status:"unknown"`, восстановление Alt+Numpad.
- INP-7: публикация `injector_hints`.
- INP-1/6/12: записи `input_health`.
- INP-6: режим stroke tool из профиля.
- IMG-8/PRIV-2: Ghost-окно считается одобренным приложением.
- CTX-6/PRIV-2: причины паузы `non_approved_app`, `always_pause_window`, `late_pause` (если не будет поправки, см. раздел 5, п. 3).
- CTX-12: версия и режим драйвера планшета.
- Manifest: сводка `health`/`dropped_stills`, инвентарь изображений; `checksums.sha256` (если применим).
- Elevated: учёт Armed held readiness; итоговые диспозиции отложенных checkpoint; полные setup/stop-наблюдения; pending grant; привязка startup-импортов (`/DEPENDENTLOADFLAG`) переложена на release engineering.
- Diagnostics UIA worker.

---

## 7. Что проверено и соответствует (кратко)

- **Видео.**
  - Cursor-free WGC, 3 буфера; borrowed frame закрывается сразу после копирования; WGC→QPC по CLK-3 через i128; thumbnail-скоринг 256×144 на GPU.
  - Native size без растяжения, паддинг до чётного размера.
  - H.264 High без B-кадров, GOP = 2·fps, peak-constrained VBR 12/20 Mb/s; IDR/SPS/PPS на границах.
  - Публикация `.part` → Finalize → `sync_all` → rename; новый сегмент после паузы с segment-local PTS; отказ энкодера без восстановления.
- **Stills.**
  - Селекторы before/seen/next/settled (500 мс, сброс кандидата), observe и pointer_rest, final.
  - BLAKE3-дедупликация без timestamp и курсора; один PNG-воркер; приоритеты IMG-7.
- **Пакет.**
  - Один writer на журнал, seq строго растёт, flush 1 с / 10 с.
  - Manifest v5 публикуется последним атомарно, с UTC-брекетами по эпохам; reader открывает только v5.
  - Диагностика пишется в выбранный каталог, без молчаливого перенаправления.
- **Сессия.**
  - Stop до admission приводит к Cancelled; Stop из паузы сохраняет cutoff; авторизация Complete против дедлайна.
  - Low-disk 20/5/6 GB; хоткеи с `MOD_NOREPEAT` и проверкой через `ToUnicodeEx`; UI не блокируется.
- **Ввод.**
  - Helper запускается с существующим токеном, `HANDLE_LIST`, job KILL_ON_CLOSE, CREATE_SUSPENDED.
  - Каждая запись потребляется не более раза; GetCursorPos отделён от позиции события.
  - Синтетические up при паузе и sync при resume; редакционный буфер 500 мс; давление пера не синтезируется.
- **Контекст.**
  - UIA на MTA-потоке, 250/500 мс, привязка к generation/revision, проверка `IsHungAppWindow`.
  - IME через `SendMessageTimeoutW`; clipboard opt-in на отдельном воркере, пишется `clipboard_change`, а не paste.
- **Elevated.**
  - Trust без заглушек: ProgramFilesX64, NTFS, ACL, Authenticode + SPKI-пин + role/release; `DeploymentRequired` для dev.
  - Reciprocal linked-token; per-lane HMAC; supervisor с lease/witness/terminate; one-flight launcher без relaunch; failed package показывается только после публикации манифеста.
- **Общее.** Строки на английском, кириллицы в коде нет, `todo!`/`unimplemented!` нет. `unsafe impl Send/Sync` только в `elevated/src/win.rs:92-93` и `pipe.rs:276`, с обоснованием.

---

## 8. Предлагаемый порядок работы

1. **Разрешить противоречия документации, раздел 5.** В первую очередь п. 1, 3, 4, 5, 6, 7, 8, 10: от них зависят I-1, I-3, C-1, S-1 и раздел 2.3.
2. **Critical/High по вводу (I-1…I-5).** Без них T7 и gates 3 не проходят.
3. **V-1 (60 fps), S-1 (suspend), P-1 (frames gap → Failed), V-3 (декодирование на критическом пути).** Это обычные сценарии, которые сейчас ломают всю попытку.
4. **E-1…E-3.**
5. **Сквозная тема 2.1 (молчаливые потери):** единый механизм учёта count и first/last с причиной.
6. **T-1/T-2:** тестовый продюсер Save, проверки gates 1–4 на `validate_v5_artifacts`. Нужно отдельное разрешение на запуск тестов.
