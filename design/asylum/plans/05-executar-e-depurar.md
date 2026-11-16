# 05 · Executar e depurar (tasks + debugger)

**Boards:** `20 · Executar e depurar` → "Executar — seletor na status bar (aberto)",
"Dock — Execuções (rodando, falhou, recentes)", "Editor — Task (Metro + iOS, etapas e pronto quando)",
"Dock — Depurar (pausado em breakpoint)", "Depurar — estados (conectar, sem Metro, dois apps, travou, caiu)",
"Specs — Executar e depurar".

## Problema

O usuário parou de usar tasks e debugger: difíceis de achar, schema limitado, "spawn que sempre
quebrava", controles que não respondiam. As causas estão no código (levantamento de 2026-11-16):

### Tasks
- `allow_concurrent_runs: false` (padrão) + terminal existente com o mesmo `full_label`: o spawn novo
  vai para `deferred_tasks` e espera a anterior terminar (`terminal_panel.rs:688-736`, `wait_for_terminals_tasks`
  ~1428). Para `expo start` isso é para sempre, sem aviso. `deferred_tasks.insert` com o mesmo id cancela o anterior.
- Args juntados sem aspas no POSIX (`util/src/shell.rs:235`, `build_no_quote`); comando sempre em shell
  interativo `-i -c` (`shell_builder.rs:32`).
- Variável `ZED_*` sem valor → `resolve_task` devolve `None` (`task_template.rs:416-425`) e
  `schedule_task` não faz nada, sem mensagem (`workspace/src/tasks.rs:51-61`).
- Saída ≠ 0 só vai para log debug. Não existe **Parar** para task (só fechar a aba); `kill_active_task`
  (`terminal.rs:3146`) só é usado pelo agente e pelo debugger.
- `.vscode/tasks.json` com `dependsOn` é descartado com um `warn` (`vscode_format.rs:94`).
- Fork — dock Devices (`crates/sidebar/src/sidebar.rs` ~11606-11800): `yarn ios --device <udid>`,
  `yarn expo start` (sem `--dev-client`, o script `start` do Trix tem), `yarn android`; `SpawnInTerminal`
  montado à mão e `spawn_in_terminal(...).detach()` (erro some, não entra no histórico → Rerun da aba abre
  o modal, `tasks_ui.rs:169`); app aberto após timer fixo de 3 s; caminho Android retorna em silêncio.

### Debugger
- Nenhuma requisição DAP tem timeout (`crates/dap/src/client.rs:100-129`); só o connect TCP
  (`transport.rs:651`). Pendentes só falham quando o stream do adaptador fecha.
- Estado otimista: step marca `Stepping`, continue marca `Running` antes de enviar
  (`project/src/debugger/session.rs:2337-2392`); se a resposta não vem, botões ficam desabilitados.
- `restart` ignorado enquanto existe `restart_task` (`session.rs:2217`); `shutdown` só mata o adaptador
  depois que terminate/disconnect responde (`session.rs:2292-2299`) → Parar trava junto.
- Toolbar usa o status da thread selecionada, padrão `Exited` (`debugger_panel.rs:685-689`); sessão
  pai do js-debug não tem thread → tudo desabilitado. Handlers de ação só registrados no estado certo
  (`debugger_ui.rs:147-270`) → atalhos não fazem nada, em silêncio.
- Erros com `.log_err()`/`.ok()`/`let _ =` em pause, disconnect, step, continue; boot/restart só no
  console de debug (`debugger_panel.rs:282-293`). Único toast: breakpoint.
- `build` do debug.json espera a task **terminar** (`running.rs` ~1250) → build "expo start" nunca libera.
- RN adapter (`dap_adapters/src/react_native.rs`): attach-only, `curl /json/list` 30× a cada 2 s,
  pega o **primeiro** alvo (iOS e Android juntos = sorte), progresso só no console, versão do js-debug
  com `log_err` + mensagem enganosa (:318-331).
- Sem i18n em `crates/debugger_ui`. Painel no dock de baixo por padrão (`default.json:3135`).

## Modelo proposto

- **Execução** = task (ou cadeia de etapas) + alvo (device) + "pronto quando". Pode ser depurada.
- Fontes no mesmo seletor: tasks.json (global, `.zed`, `.vscode`), scripts do `package.json`
  (já viram tasks em `languages/src/typescript.rs:296-318`, tag `package-script`), perfis do `eas.json`
  (novo), debug.json.
- Chaves novas no `tasks.json` (serde do Zed ignora chaves desconhecidas; atualizar o JSON schema):
  `steps`, `ready_when` (`{ "port": 8081 } | { "output": "regex" } | "exit"`),
  `if_running` (`reuse | restart | parallel`), `device` (`selected | ask | <id>`), `favorite` (1–9).
- Variáveis novas: `ASYLUM_DEVICE_ID`, `ASYLUM_DEVICE_PLATFORM`, `ASYLUM_METRO_PORT`.
- Debug RN = execução "Metro" com `if_running: reuse` + attach no app do device selecionado.

## Fase 0 — confiabilidade, sem UI nova (fazer primeiro)

Tasks:
1. Spawn em espera vira visível: em vez de ficar em `deferred_tasks` calado, toast/aviso
   "'X' já está rodando — Reiniciar · Rodar outro · Cancelar". Padrão para tasks longas: reiniciar.
2. Ação `task::Stop` (e `task::Restart`) chamando `kill_active_task`; botão na aba do terminal ao lado do rerun.
3. Variável faltando → toast dizendo qual; saída ≠ 0 → aviso na aba (não só log).
4. Args: quotar cada arg (`shlex::try_quote`) em vez de `build_no_quote`; teste com arg com espaço.
5. Dock Devices: gerar `TaskTemplate` a partir dos scripts do projeto (`start`, `ios`, `android`; detectar
   gerenciador pelo lockfile como o typescript.rs já faz), passar pelo `schedule_task` normal (histórico,
   toast, rerun), esperar o Metro responder em `:8081/status` antes de abrir o app (no lugar do timer de 3 s).
   Erros dos `return` silenciosos viram `ios_simulator_error`/equivalente Android.

Debugger:
6. Timeout por requisição em `DebugAdapterClient::request` (config `debugger.request_timeout`, padrão 10 s);
   ao estourar, devolver erro e voltar o estado da thread (`on_step_response`/`on_continue_response` já tratam erro).
7. `shutdown`: mandar terminate/disconnect com timeout curto e **sempre** matar o processo depois.
8. `restart_task` preso: limpar quando a requisição estoura.
9. Toolbar: status da sessão = melhor estado entre sessão e filhas (sessão pai do js-debug); ações
   registradas sempre, mostrando motivo quando não se aplicam.
10. Erros de step/continue/pause/disconnect/boot → toast curto + console (trocar `.log_err()` por surfacing).
11. RN adapter: progresso como status da sessão (não só console); erro de versão do js-debug honesto.

## Fase 1 — seletor + dock Execuções

1. Zona "Executar" na status bar: seletor (execução ativa + device), ▶, depurar. Popover com busca,
   FAVORITAS (⌥1–9), DO PROJETO (scripts, eas.json, tasks.json com chip de origem e último resultado),
   DEPURAR. ↵ executa, ⌘↵ depura, ⌘S favorita. Reaproveita o delegate do `tasks_ui` modal.
2. Dock "Execuções" (dock direito, junto do Agent; `paineis-no-dock-direito`): RODANDO (card com etapas,
   Saída/Reiniciar/Parar), RECENTES com `status_pill` (ok, falhou · exit N, parado por você,
   já rodando · reaproveitado), resumo do erro (primeiras linhas com caminho:linha clicável), FAVORITAS.
3. Fonte nova: perfis do `eas.json` → `eas build --profile <p> --platform <ios|android>`.
4. Atalhos no keymap JSON (`assets/keymaps/default-macos.json`, não por código) — conferido em 2026-11-16:
   - `cmd-shift-r` → seletor (hoje `task::Spawn`, mesmo papel; notebook usa o mesmo só no contexto dele).
   - `ctrl-alt-1…9` → favoritas (livres). **Não** `alt-1…9`: no macOS ⌥+número digita caractere no
     editor; `cmd-alt-1…9` já é `notes::PastePinned`. Os boards mostram "⌥1": ler como ⌃⌥1.
   - `f5` → executar a execução ativa / continuar quando pausado (substitui `debugger::Rerun`, que hoje
     ocupa o `f5` global).
   - `cmd-f5` → depurar (livre).
   - `shift-f5` → parar execução ou sessão (hoje `debugger::Stop` só no contexto de sessão; ampliar).
   - O "Run ⌘R" da dock Devices no board 08 está errado: `cmd-r` é `workspace::ToggleRightDock`. Usar F5.

## Fase 2 — etapas e editor de task

1. `steps`, `ready_when`, `if_running`, `device` no `TaskTemplate` (crate `task`) + resolução + JSON schema.
2. Executor de cadeia: etapa N só começa quando N-1 está "pronta" (porta abre / regex na saída / exit 0);
   falha para a cadeia e marca a etapa. Etapas "do Asylum" (abrir app no device) sem comando.
3. `build` do debug.json aceita task com `ready_when` (não espera terminar).
4. Editor visual (aba no editor): etapas, pronto quando, se já rodando, device, pasta, ambiente, saída,
   favorita, avisar; "Editar JSON" abre o arquivo; salva sozinho.

## Fase 3 — dock Depurar

1. Painel do debugger no dock direito como aba "Depurar" (permitir posição direita; hoje `Devices` bloqueado
   em `debugger_panel.rs:1533`): sessão (alvo + device + pill de estado), controles sempre visíveis
   (Continuar primário, passos, Reiniciar, Parar perigo), "Parou em X · arquivo:linha · Ir",
   PILHA (só código do projeto, frames de node_modules/react-native recolhidos), VARIÁVEIS, OBSERVAR,
   BREAKPOINTS, CONSOLE com avaliar no frame.
2. Estados A–E do board: conectando (passos + tempo), sem Metro (Subir Metro e conectar), dois apps
   (escolha, padrão = device da dock Devices; RN adapter filtra `/json/list` pelo device), não respondeu
   (botão em andamento com spinner, banner após timeout, Esperar mais / Forçar parar / Log),
   conexão caiu (reconectar após reload mantendo breakpoints).
3. i18n do `debugger_ui` (hoje sem `t!`).

## Fase 4 — agente

Tools `run_task`, `task_output`, `stop_task`, `debug_state` (com confirmação onde escreve);
"Corrigir com agente" no card de falha manda a saída do erro para uma thread.

## Pronto quando

- Fase 0: rodar `expo start` duas vezes nunca fica mudo; Parar sempre para (task e debug); nenhum
  erro de spawn/DAP fica só no log. Testes: quoting de args, timeout de requisição DAP, shutdown com
  adaptador que não responde (adapter fake).
- Fases 1–3: comparadas com os boards lado a lado; F5 no Trix sobe Metro + build + abre no iPhone e
  ⌘F5 para no breakpoint do `PinScreen.tsx`.

## Riscos

- `terminal_panel`, `dap` e `session.rs` são upstream e mudam muito: manter mudanças pequenas e
  isoladas, com teste, para sobreviver aos merges.
- Timeout padrão não pode matar requisições legitimamente longas (evaluate pesado): timeout por tipo
  de requisição, e "Esperar mais" em vez de cancelar sozinho.
