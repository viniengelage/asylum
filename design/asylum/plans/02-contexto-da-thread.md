# 02 · Contexto da thread

**Boards:** `14 · Notas` → "Threads — lista com contexto + Editar thread",
"Agente — contexto da thread no topo (com e sem)", "Specs — Contexto da thread".

## Problema

O usuário se perde entre as threads: o título sozinho não diz para que a thread serve nem onde parou.

## Decisão de modelo

O contexto é **uma nota do sistema de Notas ligada à thread**, não um campo novo no banco do agente.

- Arquivo `.md` em `project_notes_dir(root)` (hoje `.zed/notes/`), frontmatter `thread: <session_id>`.
- `notes::Note.thread` / `Frontmatter.thread` / `NewNote.thread` já existem em
  `crates/notes/src/store.rs` e não são usados.
- Uma nota por thread. Thread sem worktree → nota global (`global_notes_dir()`).
- Ganha de graça: aparece no dock Notas, `@nota` no composer, busca da paleta de notas.

## Onde mexer

- `crates/notes/src/store.rs`: índice `session_id → Note` no `NotesStore`; `note_for_thread`,
  `upsert_thread_note(session_id, title, body)` (cria/atualiza; corpo vazio apaga o arquivo).
- `crates/sidebar/src/sidebar.rs`:
  - `render_thread` (~7118) e o `rename_button` (~7230): o lápis passa a abrir o popover;
    F2/duplo clique continuam com `start_renaming_entry` (inline).
  - linha de contexto embaixo do título;
  - filtro (~2702, `fuzzy_match_positions`) também casa com o corpo da nota.
- `crates/agent_ui/src/agent_panel.rs`: card de contexto embaixo da barra da thread.
- Popover novo (arquivo novo em `crates/sidebar/src/` ou `crates/notes/src/thread_context.rs`,
  reutilizável pelo agent_panel): título + editor multiline markdown.

## Fase 1 — popover no lápis + lista + busca

1. `NotesStore`: índice por thread (montado ao ler/assistir as pastas), `note_for_thread`,
   `upsert_thread_note`, `delete_thread_note`.
2. Popover "Editar thread" (420 px, `bg/overlay`, borda `border/strong`, como "Guardar no bloco"):
   - campo Título (renomeia a thread pelo mesmo caminho do rename inline);
   - campo Contexto: `Editor` multiline, markdown, foco inicial nele;
   - salvamento automático com debounce (~500 ms) → "salvo agora";
   - rodapé: "Abrir em Notas" (ghost), "Pronto ⌘↵" (primary). Esc fecha.
   - "Resumir conversa" fica para a fase 3 (botão escondido até lá).
3. Lista: com contexto, até 2 linhas do primeiro parágrafo (markdown removido), `text/muted` 11.5.
4. Busca da sidebar procura também no contexto.
5. Arquivar não apaga a nota. Apagar a thread pergunta se apaga a nota junto.

**Feito em a4857e2d82:** o "Editar thread" abre como modal do workspace (`notes::ThreadContextEditor`),
não como popover ancorado ao lápis: o lápis só existe com o mouse sobre a linha, e o popover fecharia ao
mover o mouse para ele. A chave da nota é o `ThreadId` (`to_key_string`), não o `session_id`.

## Fase 2 — topo da conversa

1. Card embaixo da barra da thread no agent_panel:
   - aberto: ícone notepad, "Contexto", "Editar", chevron; markdown renderizado; "editado há X";
   - recolhido: uma linha (primeira linha + "+N linhas");
   - sem contexto: linha discreta "+ Adicionar contexto ⌥⌘E".
   - aberto/recolhido lembrado por thread (KV store / metadata da thread).
2. Ação `agent: edit thread context` com ⌥⌘E (conferir conflito no keymap padrão e nos do fork
   antes; atalho vai no keymap JSON, não por código) e item "Contexto da thread" no menu (…).

## Fase 3 — resumir + Notas

1. "Resumir conversa": usa o modelo de títulos; prompt pede objetivo, decisões, o que falta.
   Resultado entra no campo para editar; com texto existente, adiciona ao fim. Nunca grava sozinho.
2. Dock Notas: chip "thread" nas notas ligadas; clicar abre a thread.

## Pronto quando

- Dá para escrever o contexto pelo lápis e ver na lista e no topo da conversa.
- A nota existe em disco, aparece no dock Notas e no `@nota`.
- Busca encontra a thread pelo contexto.

## Dúvidas para quando chegar lá

- Com o plano 03, as notas de projeto podem migrar de `.zed/notes/` para `.asylum/notes/`
  (os learnings já foram para `.asylum/` em eaab3fb822). Se acontecer, ler das duas pastas.
- Threads de agentes externos (Claude Code, Codex): o `session_id` precisa ser estável entre
  reaberturas — conferir antes de usar como chave.
