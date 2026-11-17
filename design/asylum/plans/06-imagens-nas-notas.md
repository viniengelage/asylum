# 06 · Imagens nas notas

**Boards:** `14 · Notas` → "Notas — dock com imagens",
"Editor — nota com imagem (colar ⌘V, arrastar, bloco inline)",
"Agente — @nota com imagens e rascunho com imagem", "Specs — Imagens nas notas".

## Ideia

A nota continua um `.md` comum. A imagem vira arquivo em `assets/` ao lado e uma linha
`![alt](assets/<slug>-<n>.png)`. O Asylum mostra a imagem, leva para o agente e não a perde ao
estacionar ou colar.

## Estado atual no código

- Notas: `crates/notes/src/` — `store.rs` (`project_notes_dir` = `<root>/.zed/notes`,
  `global_notes_dir`), `panel.rs` (dock; `render_note` mostra só a primeira linha; `edit` abre o `.md`
  com `open_abs_path` no editor comum), `quick_note.rs` (nota rápida ⌥⌘N, `Editor::auto_height`).
- `@nota`: `agent_ui/src/mention_set.rs:546` `confirm_mention_for_note` lê o arquivo e devolve
  `Mention::Text` — imagens não vão. `Mention` tem `Text | Image(MentionImage) | Link` (:45).
- Imagens no agente já existem: `paste_images_as_context` / `insert_images_as_context`
  (`mention_set.rs:1048`, :1200); suporte por modelo em `LanguageModel::supports_images`
  (`language_model.rs:333`).
- Estacionar prompt: `message_editor.rs:972` `park_in_notes` salva só `self.text(cx)`.
- Screenshot da dock Devices: `sidebar.rs` ~8620 (`screenshot_android_emulator` / `screenshot_ios_simulator`).
- Editor aceita blocos customizados (`BlockProperties`, usados em diagnostics/hover).
- `markdown_preview` já renderiza imagens (`RetainAllImageCache`), útil como referência de carregamento.

**Descoberto em 2026-11-16:** o editor do Zed já salva imagem colada num `.md` ao lado do arquivo e insere
`![…](…)` (`editor/src/clipboard.rs`, `paste_item`). Nas notas isso já funciona (fica ao lado da nota, não
em `assets/`). Falta o bloco inline e a nota rápida.

**Feito em aad1535a6a:** `Mention::Note { content, images }` — `@nota` manda as imagens locais da nota.

## Fase 1 — colar e arrastar + bloco inline

1. `notes::assets_dir(note_path)` + `save_image(note, bytes, ext) -> relative_path` (nome
   `<slug>-<n>.<ext>`, n = próximo livre; PNG para clipboard). Limite 10 MB com erro visível.
2. Paste no editor de nota: interceptar `editor::Paste` quando o buffer é uma nota (caminho dentro de
   uma pasta de notas) e o clipboard tem `ClipboardEntry::Image` → salvar e inserir a linha markdown.
   Toast "Imagem colada · salva em assets/… · Desfazer" (desfazer apaga o arquivo e a linha).
3. Drop de arquivo de imagem no editor da nota → copiar para `assets/` e inserir a linha; área
   "Solte para anexar à nota" enquanto arrasta.
4. Bloco inline: para cada `![…](caminho relativo)` resolvido num arquivo existente, um bloco abaixo
   da linha (até 240 px de altura, legenda com nome · dimensões · tamanho, ações abrir/copiar/remover,
   recolher/mostrar lembrado por imagem). Arquivo faltando → bloco "imagem não encontrada".
   Começar só em notas; depois decidir se vale para qualquer `.md` (setting).
5. Nota rápida (⌥⌘N): aceitar paste de imagem (miniatura no popover; salva junto com a nota).

**Feito em dd17b31b15:** colar imagem num `.md` de nota (global ou de projeto) salva em `assets/<nota>-<n>.<ext>`
(gancho `editor::MarkdownImagePasteHandler`, limite 10 MB) e insere o link; bloco inline com a imagem
(até 12 linhas) ou "Imagem não encontrada". **Fica para depois:** toast com Desfazer, arrastar arquivo,
ações/recolher no bloco, nota rápida com imagem.

## Fase 2 — agente

1. `confirm_mention_for_note`: devolver texto + imagens. Mudar `Mention` para suportar composto
   (ex.: `Mention::Text { content, images: Vec<MentionImage>, .. }`) ou expandir a nota em várias
   menções; respeitar `supports_images` (sem visão: só texto + aviso discreto no composer).
2. `park_in_notes`: salvar as imagens anexadas ao composer em `assets/` e referenciá-las no corpo.
3. Retomar rascunho, "Colar no agente" e `notes::PastePinned` (⌥⌘1–9): reinserir texto **e** imagens
   (usar `insert_images_as_context`).

**Feito em dd17b31b15:** estacionar salva as imagens do compositor em `assets/` e põe os links no rascunho;
retomar devolve texto + imagens (`notes::InsertImages`). `PastePinned` continua colando só o texto.

## Fase 3 — dock e Devices

1. `panel.rs`: miniatura à direita quando a nota tem 1 imagem; galeria (até 2 + "+N") na nota
   selecionada; filtro "Com imagem"; contador "texto + N imagens" nas ações.
2. Screenshot do simulador/emulador: ação "Guardar em nota" (nova nota ou anexar à nota aberta).

**Feito em dd17b31b15:** miniatura na linha da nota e galeria (2 + "+N") na selecionada. **Fica para depois:**
filtro "Com imagem", contador nas ações, "Guardar em nota" no screenshot da dock Devices.

## Fase 4 — limpeza

- Apagar nota pergunta se apaga as imagens dela.
- Comando "notes: remove unused images" (imagens em `assets/` sem referência).

**Feito em dd17b31b15:** apagar no dock pergunta se apaga as imagens que nenhuma outra nota usa;
`notes: remove unused images` com toast do total.

## Pronto quando

- Colar um screenshot numa nota mostra a imagem no editor e o arquivo existe em `assets/`.
- `@nota` com imagens chega ao modelo com as imagens (verificar no request).
- Estacionar e retomar um prompt com screenshot devolve o screenshot.

## Riscos

- Interceptar `Paste` só no contexto de notas para não mudar o editor em geral.
- Notas podem migrar de `.zed/notes` para `.asylum/notes` (plano 03): `assets/` vai junto, links relativos
  continuam válidos.
