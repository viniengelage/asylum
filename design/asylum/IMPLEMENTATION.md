# Asylum — guia de implementação do redesign

> Fork pessoal do Zed. Este documento traduz o redesign feito no Penpot em mudanças concretas no
> código. Ele **não** substitui `crates/ui/.rules`; onde o redesign contradiz uma regra de lá, isso
> está marcado como **⚠ decisão** e a regra precisa ser atualizada junto com o código.

- **Fonte da verdade visual:** arquivo Penpot (Page 1), boards:
  - `Workspace — Polar` (Editor + Terminal, Device iOS, Agent) — estado base
  - `Workspace — Git` (dock na aba Git, foco no dock)
  - `Workspace — Threads` (sidebar de threads como ilha, sem Device)
  - `Workspace — JSON Prettier` (toolkit aberto no dock)
  - `Specs — controles` (estados de Tab, IconButton, menu, tooltip, empty state do Git)
  - `Brand — Asylum` (ícone, lockups, variantes, paleta)
- **Tema:** `~/.config/zed/themes/polar-dark.json` (Polar Dark). O Penpot tem o set de tokens
  `polar-dark` espelhando esses valores.
- **Marca:** `design/asylum/brand/*.svg`.

---

## 1. O problema que o redesign resolve

| # | Sintoma | Causa no código/tema |
|---|---------|----------------------|
| 1 | Aba ativa quase igual à inativa (Android/iOS, Agent/Git, abas de arquivo) | A regra de continuidade (`Tab::surface` = fundo do conteúdo) só funciona quando os degraus da *surface ladder* são visíveis. No Polar Dark: `tab.active_background #08070C` vs `tab.inactive_background #0E0D14` — ~2% de luminância. A aba ativa "desaparece" no conteúdo em vez de se destacar. |
| 2 | `×` em todas as abas do dock | Não fica claro o que está ativo nem o que o `×` fecha. |
| 3 | Status bar com ~20 ícones sem grupo | Info do editor, toggles de painel e toolkit misturados; ferramenta ativa só muda a cor do ícone. |
| 4 | Toolbar do simulador sem hierarquia | 7 ícones de mesmo peso; Power (destrutivo) colado nos demais; Run sem destaque. |
| 5 | Estado do device como texto ("Inicializado") | Sem indicador visual. |
| 6 | Não se vê qual ilha tem foco | `pane.focused_border #6D28D9` existe no tema e não é usado nos cards. |
| 7 | Sidebar de threads não é ilha | Não usa `workspace_card`, quebra o grid. |

## 2. Princípios

1. **Ilhas continuam sendo a linguagem.** Todo pane, dock, sidebar e a status bar são cards
   (`StyledExt::workspace_card`) separados pelo `background` (gap = `workspace_card_gap`, que lê o
   setting `card_gap`; o design usa 8px).
2. **Seleção é explícita, não por continuidade.** Qualquer coisa selecionada combina três sinais:
   fundo elevado + borda accent + ícone accent (e peso 600 no texto). Nunca só a cor do texto.
3. ~~**Foco é da ilha.** A ilha que recebe input ganha a borda `pane.focused_border` a 70%.~~
   **Decidido:** sem borda de foco nas ilhas — todas usam `border`. O foco já aparece no cursor,
   na linha ativa e na aba ativa.
4. **Ações têm peso.** Uma ação primária por ilha (Run, Send, Commit, Formatar, Nova thread) em
   `accent.strong`; secundárias agrupadas em *clusters* com fundo `elevated`; destrutivas isoladas
   e em `error`.
5. **Grid de 4px, alvos ≥ 28px** (24px só na status bar).

---

## 3. Tokens → tema

Valores do Polar Dark. A coluna "tema" é a chave do `polar-dark.json` que deve carregar o valor.
Chaves marcadas **novo** não existem hoje — ver §3.1.

| Token (Penpot) | Valor | Uso | Chave do tema |
|---|---|---|---|
| `color.bg.editor` | `#08070C` | shell da janela / gap entre ilhas, conteúdo do editor e terminal | `background`\*, `editor.background`, `terminal.background` |
| `color.bg.app` | `#0E0D14` | fundo das ilhas (chrome) | `panel.background`, `tab_bar.background`, `status_bar.background`, `title_bar.background` |
| `color.bg.elevated` | `#13111B` | clusters de botões, inputs, composer, seletores | `elevated_surface.background`, `element.background` |
| `color.bg.hover` | `#1F1C2B` | hover, chips neutros | `element.hover`, `ghost_element.hover` |
| `color.bg.active` | `#322C46` | fundo da **aba ativa (pill)** | `element.selected`, `ghost_element.selected` |
| `color.bg.overlay` | `#181622` | menus, popovers, tooltips | **novo** (ou reutilizar `elevated_surface.background`) |
| `color.accent.soft` | `#2A1F3F` | linha selecionada (árvore, threads), icon button ativo, badges | **novo** `element.selected_accent` — opaco ≈ `#A855F7` a 20% sobre `bg.app` |
| `color.accent.default` | `#C084FC` | ícone/texto accent, cursor, indicadores | `text.accent`, `icon.accent` |
| `color.accent.strong` | `#6D28D9` | botão primário, borda de seleção/foco | `border.focused`, `pane.focused_border` |
| `color.border.default` | `#1F1C2B` | borda das ilhas, separadores | `border` |
| `color.border.strong` | `#322C46` | borda de input/composer, divisores | `border.selected` |
| `color.text.default` | `#EDEAF5` | texto primário | `text` |
| `color.text.body` | `#C7C2D6` | texto de lista/código | `editor.foreground`, `icon` |
| `color.text.muted` | `#9C96AE` | texto secundário, ícones inativos | `text.muted` |
| `color.text.subtle` | `#837C98` | labels, placeholders, metadados | `text.placeholder`, `icon.muted` |
| `color.text.faint` | `#322C46` | números de linha, desabilitado leve | `editor.line_number` |
| `color.status.*` | success `#BEF264` · error `#FB7185` · warning `#E8B368` · info `#60A5FA` | status | `success`, `error`, `warning`, `info` |
| `space.1..8` | 4 · 8 · 12 · 16 · 20 · 24 · 32 | — | `DynamicSpacing` (Base04, Base08…) |
| `radius.sm/md/lg` | 6 · 8 · 12 | botão · strip/input · ilha | `rounded_md` / `pane_corner_radius()` (hoje 0.5rem = 8px)\*\* |

\* No Penpot o gap entre ilhas é `#08070C` (mais escuro que as ilhas). Hoje `background` é
`#0E0D14`, igual às ilhas — **mudar `background` para `#08070C`** é o que faz as ilhas "saltarem".

\*\* O design usa 12px no card da ilha e 8px nos strips/inputs internos. Se quiser manter
`pane_corner_radius()` único, 8px também funciona — mas tabs-pill internas precisam de um raio
menor que o do strip (6px dentro de 8px), senão os cantos colidem.

### 3.1 Chaves novas no tema
`crates/theme/src/styles/colors.rs` já documenta a *surface ladder*. Adicionar (com
fallback nos temas embutidos para não quebrar outros temas):

- `element.selected_accent` (fundo de seleção com matiz accent) → fallback `element.selected`
- `elevated_surface.overlay` (menus/tooltips) → fallback `elevated_surface.background`

Ambas são opcionais: dá para começar só com as chaves existentes e `.opacity()` sobre `text.accent`.

---

## 4. Componentes

Medidas em px na densidade Default. Em código use `DynamicSpacing` / métricas do `HeaderBar`
(regra do `.rules`), não literais.

### 4.1 Tab — variante *Pill* (⚠ decisão)
Usada em **todos** os strips de ilha: abas de arquivo, Terminal, switcher do dock
(Agent/Git/JSON), plataforma (Android/iOS/Browser), sub-abas (Changes/History), modo do JSON,
escopo das Threads.

```
strip:  bg=editor.background  border=1 border  radius=8  padding=3  gap=2
tab:    h=26  px=10  gap=6  radius=6
  inativa: bg=none            icon=text.subtle 14px  label=text.muted 12.5/500
  hover:   bg=ghost_element.hover
  ativa:   bg=element.selected  border=1 border.focused@55%  icon=text.accent  label=text 12.5/600
  close:   14px slot, só na ativa ou em hover (TabCloseSide::End)
  status:  ponto 6px (ex.: terminal rodando = success)   |   badge de contagem (accent.soft + text.accent)
```

**⚠ Conflito com `crates/ui/.rules`:** a regra "A selected `Tab` must be told the surface it
covers, via `Tab::surface`" existe para a aba ativa se fundir ao conteúdo. O redesign faz o
oposto para strips de ilha. Proposta:

- Adicionar `TabStyle { Surface, Pill }` em `crates/ui/src/components/tab.rs` (default `Surface`
  para não quebrar call sites). `Pill` ignora `surface`, pinta `element_selected` + borda
  `border_focused` a 55% na ativa e `ghost_element_hover` no hover da inativa.
- `TabBar` (`tab_bar.rs`) ganha `.style(TabStyle::Pill)`, que envolve as tabs no strip
  arredondado (padding 3, gap 2) e propaga o estilo para cada `Tab`.
- Atualizar `.rules`: "strips de ilha usam `TabStyle::Pill`; `Surface` fica para casos em que
  a aba precisa continuar o conteúdo".
- O `full_width` já existente atende Android/iOS/Browser e Changes/History (tabs que dividem a largura).

### 4.2 IconButton
```
tamanho: 28×28 nas ilhas · 24×24 na status bar · 22–26 em strips densos
ícone:   16 (ilhas) / 14 (status bar, strips) — traço 1.5
estados: default  icon=text.muted, sem fundo
         hover    bg=ghost_element.hover
         active   bg=accent.soft, icon=text.accent      ← ferramenta aberta / painel visível
         danger   icon=error (Power)
radius:  6
```
Continua valendo o `.rules`: dentro de `Tab` → `ButtonSize::None` + `IconButtonShape::Square`.

### 4.3 Button primário / split button
- Primário: `h=30 px=12 radius=8 bg=border.focused (#6D28D9) text=text 12.5/600`, ícone 12–13px,
  atalho opcional a 60% de opacidade (`⌘R`, `⌘↵`). Contraste texto/fundo ≈ 6.5:1.
- Split button (`Pull ▾`, `Stage All ▾`, `Commit ▾`): ação + divisor 1px + chevron 26px, mesmo raio.
  Secundário usa `bg.elevated` + `border`.

### 4.4 Cluster de botões
Grupo de icon buttons relacionados: `bg=elevated border=1 radius=8 padding=2 gap=2`. Separa
função sem precisar de divisores soltos.

### 4.5 Status chip / Kbd / Tooltip / Menu
- **Chip:** `h=20–22 radius=full px=8 gap=6`, ponto 6px + texto 11/500 na cor do status; fundo
  `bg.hover` ou a cor de status a 10–12%.
- **Kbd:** `h=18 px=5 radius=4 bg=bg.hover` texto 10.5. Use fonte de UI (os glifos ⌘ ⌥ ⇧ não existem em IBM Plex Mono).
- **Tooltip:** `h=28 px=10 radius=6 bg=overlay border=border.selected`: ícone + nome + atalho.
- **Menu:** `w≈264 padding=4 radius=8 bg=overlay border=border.selected` + sombra `0 8 24 #000@50%`;
  item `h=30 px=8 radius=6`, ícone 14 + label + atalho à direita; hover `bg.hover`.
  Referência: `render_device_settings_menu` (`crates/ui/src/components/context_menu.rs` já foi mexido no fork).

### 4.6 Linha selecionada (árvore, threads, arquivos do Git)
`bg=accent.soft border=1 border.focused@60% radius=6`, ícone `text.accent`, label `text` 500.
Hover de linha não selecionada: `bg.hover`. Linha de lista = 26–28px.

---

## 5. Implementação por área

Ordem sugerida = ordem das seções. Cada item cita o arquivo onde a mudança mora hoje.

### 5.1 Cards, gap e foco — `crates/ui/src/traits/styled_ext.rs`, `crates/workspace/src/pane_group.rs`
- `workspace_card` aplica `rounded(pane_corner_radius()) + border_1 + border + overflow_hidden`.
  Sem variante de foco (ver §2.3).
- Tema: `background: #08070C` (§3).
- Nova coluna central: Editor e Terminal são dois cards empilhados com o mesmo
  `workspace_card_gap`, não um único card com divisória.

### 5.2 Status bar global — `crates/workspace/src/status_bar.rs`, `crates/toolbox/src/toolbox.rs`
- A status bar vira um card de largura total **abaixo** de todas as ilhas. Em `workspace.rs` isso já
  acontece no ramo *unified panes* (todo painel é tab no `center`); no ramo com docks ela é filha da
  coluna central, abaixo do `bottom_dock`. ⚠ **decisão:** mover o `.child(self.status_bar.clone())`
  desse ramo para o container raiz, igual ao ramo unificado.
- Três zonas (`render_left_tools` / `render_right_tools` + uma zona de toolkit):
  1. **Esquerda:** toggles de dock (left/bottom/right, estado *active* quando visíveis) · divisor ·
     diagnóstico (✕ n, ⚡ n) · branch · status do LSP.
  2. **Direita, editor:** `25:19 · LF · UTF-8 · TypeScript` (cada um é botão 24px com hover).
  3. **Direita, toolkit:** label `TOOLKIT` (10/600, tracking 0.8, `text.subtle`) + um icon button por
     ferramenta; a ferramenta cujo painel está aberto fica *active*. Hoje `ToolboxButton` renderiza um
     único `IconButton` (`IconName::ToolHammer`) com popover — trocar por botões diretos (JSON, Browser,
     Terminal, Debug, Wrench…) e manter o popover só para ferramentas que não cabem.
  4. **Direita, painéis:** Device · Agent · Git como toggles *active*.
- Divisores entre zonas: 1×16px `border.selected`.

### 5.3 Pane do editor — `crates/workspace/src/pane.rs`, `crates/ui/src/components/tab_bar.rs`
- Header (`HeaderBar`, level `Pane`, 44px): ← → · strip de tabs **Pill** · `+ split maximize`.
- Breadcrumb (`HeaderBar`, level `Content`, 32px): segmentos em mono 12, último em `text` 500,
  chevrons 12px `text.faint`; à direita buscar / assistente / opções como icon buttons 24px.
- Linha ativa: `editor.active_line.background` (já `#6D28D914`). Blame inline em `text.subtle` a 80%.

### 5.4 Terminal — `crates/terminal_view/src/terminal_panel.rs`
- Card próprio embaixo do editor (300px, redimensionável).
- Header 40px: tabs Pill por sessão (`expo start` com ponto `success` enquanto roda, `zsh`, `git`) +
  `+` · split · maximize · fechar à direita.
- `TerminalView` já implementa `Item::content_background` (ver `.rules`); o conteúdo usa
  `terminal.background`.

### 5.5 Devices — `crates/sidebar/src/sidebar.rs` (`SidebarView::Devices`)
- Header: tabs Pill `full_width` **Android · iOS · Browser** + `⋯`. ⚠ **decisão:** Browser (web_preview)
  entra como terceira plataforma aqui em vez de ser um item à parte.
- Device bar (`render_android_device_selector` / seletor iOS): seletor `h=30 bg=elevated radius=8` com
  ícone do device, nome (13/500), versão do SO (12 `text.subtle`), chip de status
  (`Running` = success, `Booting` = warning, `Shutdown` = text.subtle) e chevron; refresh à direita.
- Stage: device centralizado, sem a engrenagem flutuante sobre o conteúdo do app.
- Toolbar (`render_device_toolbar`, 48px, `HeaderBar::footer`):
  `[▶ Run ⌘R]` primário · cluster `[build]` · cluster `[record, screenshot]` · cluster `[paste, open]` ·
  espaço · `power` (danger) · `settings` (active quando o menu está aberto).
- Menu (`render_device_settings_menu`): Alterar localização `⌥L` · Mostrar teclado `⌘K` · Reiniciar app.
  ⚠ os atalhos são propostas.

### 5.6 Dock direito unificado — `crates/workspace/src/dock.rs`, `crates/workspace/src/panel_item.rs`
- Um strip Pill com **Agent · Git · (ferramentas do toolkit abertas)**. `×` só na aba ativa.
- `+` e `⋯` no fim do strip.

### 5.7 Agent — `crates/agent_ui/src/agent_panel.rs`
- Thread bar 40px: título 13/500 + histórico · maximizar.
- Empty state: "Sugestões" (label 10.5/600) + 3 cards de sugestão (`h=36 bg=elevated border radius=8`,
  ícone accent 14).
- Composer: card `bg=elevated border=border.selected radius=12 padding=12/12/8`:
  chips de contexto (mono 11.5, `bg.hover`) · placeholder · linha de controles
  (`+`, `@` · espaço · `Write ▾`, `Opus 5.5 ▾`, `High ▾` · Send 28px primário).

### 5.8 Git — `crates/git_ui/src/git_panel.rs`
- **Branch bar no topo** (44px) — contexto antes da ação: seletor `repo / branch` (branch em 600) com
  `↑n ↓n` (behind em `info`) + split `Pull ▾`. Hoje isso fica no rodapé.
- Sub-abas Pill `full_width`: **Changes (badge de contagem) · History**.
- Toolbar 40px: `View Diff` (ghost) · espaço · buscar · filtros · split `Stage All ▾`.
- Lista agrupada: **STAGED n** (ação "Unstage all") e **CHANGES n** ("Stage all"); linha 28px com checkbox
  14px (marcado = `accent.strong` + check), nome (13), diretório (12 `text.subtle`), ações no hover
  (abrir, histórico) e badge de status 18px (`M` warning, `A` success, `D` error, fundo da cor a 12%).
- Composer de commit (mesmo card do Agent): mensagem em mono · expandir · dica "1 arquivo staged ·
  até 72 caracteres" · `✦ Gerar mensagem` (ghost) · split `✓ Commit ▾` primário.
- Rodapé 44px: avatar 20px + último commit (12.5) + `sha · há 2 horas` (mono 11) + desfazer · branch.
- Empty state: ícone de check em círculo `bg.hover`, "Nada para commitar", "branch está N commits atrás",
  `Pull ▾` + `View Branch Diff` — em vez do texto solto no meio do painel.

### 5.9 JSON Prettier — `crates/toolbox/src/json_prettier.rs`
- Toolbar 48px: Pill **Format · Minify · Validar** · espaço · `2 espaços ▾` · toggle "ordenar chaves"
  (icon button *active*).
- Linha de validação: chip `✓ JSON válido` (success a 10%) + `18 linhas · 412 B · chaves ordenadas`.
  Erro: chip em `error` + "linha 12, coluna 8: vírgula faltando" e a linha marcada no editor.
- Editor embutido em card rebaixado (`editor.background border radius=8`), números de linha 12, JSON com
  a sintaxe do tema (chaves `info`, strings `#2DD4BF`, números/bool/null `#E8B368`).
- Rodapé 48px: `Colar` · `Limpar` · espaço · `Copiar` · `{ } Formatar ⌘↵` primário.
- Placeholder vazio: "Cole JSON aqui… ou arraste um arquivo".

### 5.10 Threads como ilha — `crates/sidebar/src/sidebar.rs` (`SidebarView::ThreadList`)
- Envolver a sidebar em `workspace_card` com `panel.background` e o mesmo gap das outras ilhas.
- Header 44px (`render_sidebar_header` / `render_filter_input`): busca `h=30 bg=elevated` com `⌘K` +
  botão primário quadrado `+` (nova thread, `render_new_thread_button`).
- Escopo: Pill `full_width` **Todos · frontend · backend** (worktrees do workspace).
- Lista (`render_thread`): agrupada por **HOJE / ONTEM / ESTA SEMANA / ANTES** (sticky header,
  `render_sticky_header`). Item em duas linhas: título 13 (truncado com `…`), metadados = chip do projeto
  (10.5, `bg.hover`) + tempo (11 `text.subtle`).
  - Ativa: linha selecionada §4.6.
  - Rodando: ponto 7px `text.accent` no fim do título e "respondendo… · 1h" em `text.accent`.
- Rodapé 40px (`render_sidebar_bottom_bar`): "42 threads" · histórico/arquivo · toggle da sidebar (*active*).

### 5.11 Project panel — `crates/project_panel/src/project_panel.rs`
- Header 44px: traffic lights · seletor de projeto (13/600 + chevron) · buscar.
- Linha 26px, indentação 16px por nível, chevron 12 + ícone 14.
- Selecionada: §4.6. Status git como letra mono 11/600 alinhada à direita (`M` em `modified`).

---

## 6. Marca — Asylum

Arquivos em `design/asylum/brand/`:

| Arquivo | Uso |
|---|---|
| `asylum-app-icon.svg` | ícone do app (gradiente `#7C3AED → #2E1065`, cela acolchoada, A + espiral) |
| `asylum-app-icon-dark.svg` | variante escura (fundo `#13111B`, A em `#C084FC`) |
| `asylum-app-icon-mono.svg` | monocromática |
| `asylum-mark.svg` | só o símbolo, sem fundo |
| `asylum-mark-currentcolor.svg` | símbolo 24px com `currentColor` — para virar um `IconName` |

**Conceito:** o "A" é um caret de código (`^`); a espiral no lugar do travessão é a loucura e o loop;
o losango com "botões" é a parede de uma cela acolchoada. Wordmark: `asylum` em minúsculas, Geist 600.
Tagline: `dev é loucura`. Abaixo de 32px o padrão acolchoado sai (só gradiente + A + espiral); em 16px
a espiral vira ponto e ainda funciona.

**Gerar o `.icns` e trocar os ícones do bundle** (macOS):
```sh
cd design/asylum/brand
mkdir -p Asylum.iconset
for s in 16 32 64 128 256 512; do
  rsvg-convert -w $s -h $s asylum-app-icon.svg -o Asylum.iconset/icon_${s}x${s}.png
  rsvg-convert -w $((s*2)) -h $((s*2)) asylum-app-icon.svg -o Asylum.iconset/icon_${s}x${s}@2x.png
done
iconutil -c icns Asylum.iconset -o Asylum.icns
# PNGs do bundle (substituem os do Zed):
rsvg-convert -w 512  asylum-app-icon.svg -o ../../../crates/zed/resources/app-icon.png
rsvg-convert -w 1024 asylum-app-icon.svg -o ../../../crates/zed/resources/app-icon@2x.png
```
(`rsvg-convert` vem de `brew install librsvg`.) Ajuste também o nome do bundle/app nas configs de
empacotamento que você usa (`script/bundle-mac`, `Info.plist`) — fora do escopo deste documento.

### 6.1 Ícone por perfil

Cada perfil roda num processo próprio (`--profile <id>`), então cada um ganha um tile próprio no
Dock. A ideia é que o ícone desse tile use a cor do perfil, para que dê para saber qual instância é
qual sem abrir a janela.

**Desenho:** board `Brand — ícone por perfil` no Penpot (x=25312, y=1160). SVGs em
`design/asylum/brand/profiles/asylum-app-icon-<cor>-macos.svg`, já no grid do macOS (corpo 824 em
1024, com sombra), iguais ao `asylum-app-icon-macos.svg` com quatro cores trocadas:

| `ProfileColor` | Swatch (chip) | Gradiente (600 → 950) | Espiral | A |
|---|---|---|---|---|
| `Amber` | `#E8B368` | `#D97706 → #451A03` | `#C084FC` (inverte para roxo) | `#FFF7ED` |
| `Teal` | `#5EEAD4` | `#0D9488 → #042F2E` | `#E8B368` | `#F0FDFA` |
| `Purple` | `#C084FC` | `#7C3AED → #2E1065` (= ícone do bundle) | `#E8B368` | `#F5F0FF` |
| `Blue` | `#60A5FA` | `#2563EB → #172554` | `#E8B368` | `#EFF6FF` |
| `Rose` | `#FB7185` | `#E11D48 → #4C0519` | `#FDE68A` | `#FFF1F2` |

Só o fundo muda: a forma continua a mesma e o ícone continua sendo o Asylum. Uma cor que ocupa o
ícone inteiro dá para distinguir a 32 px; espiral colorida ou selo no canto (alternativas B e C
no board) somem no Dock. Ao adicionar uma cor nova em `ProfileColor`, é preciso adicionar a linha
aqui, o SVG e o PNG.

**Por que o ícone em tempo de execução não basta.** `NSApp.setApplicationIconImage` só vale
enquanto o processo roda: com o app fechado, o tile fixado volta ao ícone do bundle. O Dock fixa
por bundle, então com um único `Asylum.app` não dá para fixar "Trabalho" e "Pessoal" como itens
separados, e "Manter no Dock" numa segunda instância fixa o mesmo `Asylum.app`, que abre o perfil
padrão. Para ver a diferença **antes** de abrir, cada perfil precisa de um bundle próprio.

**Lançador por perfil (validado em spike, 2026-09-24).** Um `.app` por perfil em `~/Applications`,
que é o mesmo binário visto por outro bundle:

```
~/Applications/Asylum Trabalho.app/Contents/
  Info.plist            cópia do Info.plist do Asylum.app, com:
                          CFBundleIdentifier  dev.asylum.Asylum.profile.<id>
                          CFBundleName / CFBundleDisplayName  "Asylum <Nome>"
                          CFBundleIconFile    profile
                          CFBundleExecutable  launch
                          AsylumProfile       <id>
                          sem CFBundleURLTypes e CFBundleDocumentTypes (o zed:// e os
                          tipos de arquivo continuam só no Asylum.app)
  MacOS/launch          script: exec "$(dirname "$0")/zed" "$@"
  MacOS/zed  -> /Applications/Asylum.app/Contents/MacOS/zed   (symlink)
  MacOS/cli, MacOS/git -> idem
  Frameworks -> /Applications/Asylum.app/Contents/Frameworks  (symlink)
  Resources/profile.icns       gerado do SVG da cor (iconutil)
  Resources/Document.icns -> symlink
```

O que o spike mostrou (bundle feito à mão, perfil descartável `icon-spike`, Asylum instalado):

- Se o executável do lançador fizer `exec` direto em `/Applications/Asylum.app/.../zed`, **não
  funciona**: o AppKit resolve o bundle principal pelo caminho do executável, e o processo aparece
  como `dev.asylum.Asylum`.
- Com o `exec` passando pelo **symlink dentro do lançador**, funciona: o `lsappinfo` mostra
  `bundleID = dev.asylum.Asylum.profile.icon-spike`, e o Dock ganha um tile próprio, rosa, com a
  bolinha de "rodando". Fixado, ele continua rosa com o app fechado.
- A assinatura continua válida: `codesign --verify <pid>` → `dynamically valid`, `valid on disk`,
  `satisfies its Designated Requirement`. O cdhash é o do binário real, então o Keychain vê o mesmo
  código (mas isso ainda não foi testado com um perfil que tenha credenciais).
- Web preview: o CEF sobe em modo multi-processo pelo symlink `Frameworks`, os helpers resolvem
  para o caminho real e os frames chegam na GPU.
- Clicar no lançador com o perfil já aberto só traz o mesmo PID para a frente, sem criar outra
  instância.
- **Permissões do macOS são por bundle id**: no primeiro launch, o lançador pediu de novo a
  permissão de notificações ("Notificações de 'Asylum'"). Câmera, microfone, gravação de tela e
  acessibilidade também precisam ser concedidas de novo, uma vez por perfil.
- **O executável do bundle não pode ser o symlink.** Com `CFBundleExecutable = zed` (o
  symlink), o LaunchServices resolve o link antes de executar, e o processo nasce como o app
  real, no perfil padrão. Por isso o `CFBundleExecutable` é um script de uma linha que faz `exec`
  pelo symlink irmão. O script não passa `--profile`: o perfil vem do `AsylumProfile`, então o
  restart (`open -n <lançador> --args --profile x`) não duplica o argumento.
- Teste ponta a ponta da implementação (2026-09-24, build debug num bundle de teste): o lançador
  gerado pelo sync, aberto com `open` e sem argumentos, sobe como
  `dev.asylum.Asylum.profile.spike`, com tile rosa próprio no Dock e o log em `Zed-spike.log`;
  abrir de novo só ativa o mesmo PID. Um build debug fora do checkout morre ao carregar os assets
  (`dev asset loading requires running from within the checkout`): para testar, o bundle tem que
  ficar dentro do repositório (por exemplo `target/`).

**Onde o código assume que `app_path()` é o Asylum.app** (com o lançador, `NSBundle.mainBundle`
passa a ser o lançador):

| Lugar | O que quebraria | Correção |
|---|---|---|
| `auto_update.rs` (`install_release(..., running_app_path)`) | instalaria a versão nova **por cima do lançador** | usar o app real: `current_exe()` canonicalizado, subindo até o `.app` |
| `app_profiles.rs` `launch_profile` / `open_profile` / `forward_paths` | abriria outro perfil com o bundle (e o ícone) do lançador atual | abrir o lançador do perfil de destino quando ele existe, senão o app real com `--profile` |
| `gpui_macos` `restart` | reabre o bundle atual, que na troca de perfil na mesma janela é o lançador do perfil de **origem** | `switch_in_place` chama `set_restart_path` com o bundle do perfil de destino |
| `move_to_applications.rs` | nada: só oferece mover apps em `/Volumes` ou transladados, e o lançador fica em `~/Applications` | — |
| `web_preview/cef_browser.rs` `main_bundle_path` | nada no spike; o CEF lê o Info.plist do lançador | manter no roteiro de teste |

**O que cada plataforma permite**

| | Com o app fechado (escolher qual abrir) | Com o app aberto |
|---|---|---|
| macOS | lançador `.app` por perfil em `~/Applications`, fixável no Dock e achado pelo Spotlight | o tile do próprio lançador; `setApplicationIconImage` só como reserva para perfis abertos sem lançador (`zed --profile` pela CLI) |
| Windows | atalho `.lnk` por perfil com ícone próprio e `System.AppUserModel.ID` = AUMID do perfil, fixável na barra de tarefas | o processo usa o mesmo AUMID (`set_app_identity`) e cai no grupo do atalho; `WM_SETICON` para Alt-Tab |
| Linux | `.desktop` por perfil em `~/.local/share/applications` (`Icon=`, `Exec=... --profile <id>`, `StartupWMClass`) | X11: `_NET_WM_ICON` (o gpui já envia `WindowOptions::icon`); Wayland: `app_id` por perfil casando com o `.desktop` |

**Fases**

0. **Assets** (feito). `script/export-profile-icons` gera os SVGs de
   `design/asylum/brand/profiles/` e os `.icns` em `crates/zed/resources/profile-icons/<cor>.icns`
   (iconset 16–1024, cerca de 2 MB no binário, via `include_bytes!`). O `.png` para o ícone em
   tempo de execução entra na fase 3.
1. **Perfil pelo bundle** (feito). `profile_launchers::launcher_profile_id` lê o `AsylumProfile`
   do `Info.plist` ao lado do executável (crate `plist`) quando não há `--profile` nem
   `--user-data-dir`. `util::app_bundle_path()` acha o app real pelo `current_exe()` canonicalizado;
   o auto-update instala nele.
2. **Criar e manter os lançadores** (feito, `crates/zed/src/zed/profile_launchers.rs`):
   - `ProfileStore::sync_launchers` roda sempre que o registro carregado muda (id, nome ou cor),
     inclusive no startup, e só quando o processo roda de um bundle. Grava só o que difere e
     chama `lsregister -f` quando algo mudou, então o sync também atualiza o Info.plist depois de
     um update do app.
   - Um lançador por perfil que não é o padrão. Renomear o perfil renomeia o `.app` (o item
     fixado no Dock segue); nome repetido ganha ` (<id>)`; o lançador de um perfil excluído é
     removido antes de criar os novos.
   - `launch_profile`, `open_profile`, `forward_paths`, `run_in_profile` e `switch_in_place` usam
     `profile_launchers::app_for_profile`: o lançador do perfil de destino, ou o app real.
   - Gerenciador de perfis: botão "Atalho no Finder" ao lado da pasta de dados, para arrastar o
     lançador para o Dock.
   - Falta: testar o Keychain com a versão release, abrindo um perfil com credenciais pelo lançador.
     Um build debug tem outro cdhash de qualquer jeito, então não serve para esse teste.
3. **Ícone em tempo de execução (reserva).** `Platform::set_app_icon(Option<Arc<RgbaImage>>)` no
   GPUI, no molde de `set_app_identity`; no macOS via `NSApplication::setApplicationIconImage`.
   Só entra quando o processo roda de `Asylum.app` com um perfil que não é o padrão (CLI, ou perfil
   sem lançador), e é o que o Windows e o Linux reaproveitam.
4. **Windows.** Um `.lnk` por perfil (Menu Iniciar) com o `.ico` da cor e
   `System.AppUserModel.ID = …Profile.<id>`; o processo chama `set_app_identity` com o mesmo AUMID
   e registra esse AUMID em `system_notifications.rs` (`register_app_user_model_id`), senão os
   toasts somem. `WM_SETICON` com o `.ico` para o Alt-Tab.
5. **Linux.** `.desktop` por perfil e `app_id` por perfil; no X11, `ProfileColor::app_icon()` no
   lugar do `APP_ICON` em `zed.rs`. Baixa prioridade.

**Critérios de aceite**

- [ ] "Asylum Trabalho" e "Asylum Pessoal" fixados no Dock mostram cores diferentes com o app
      fechado, e clicar abre o perfil certo.
- [ ] Com o perfil aberto, há um tile só por perfil (o do lançador, com a bolinha de "rodando").
- [ ] O ⌘-Tab mostra o ícone e o nome do perfil.
- [ ] Um perfil com credenciais no Keychain abre pelo lançador sem pedir acesso de novo.
- [ ] O auto-update substitui o `/Applications/Asylum.app`, nunca o lançador, e o lançador abre a
      versão nova sem ser recriado.
- [ ] Trocar a cor ou renomear o perfil atualiza o lançador e o item fixado no Dock.
- [ ] Os ícones têm a mesma margem e sombra do `app-icon.png`, então os tiles ficam do mesmo
      tamanho.

---

## 7. Critérios de aceite

- [ ] Em qualquer strip, a aba ativa é identificável **sem ler o texto** (fundo + borda + ícone accent).
- [ ] `×` só aparece na aba ativa ou em hover.
- [ ] Todas as ilhas usam `border`, com ou sem foco.
- [ ] Cada ilha tem no máximo uma ação primária em `accent.strong`.
- [ ] Power/ações destrutivas isoladas e em `error`.
- [ ] Alvos ≥ 28px nas ilhas, ≥ 24px na status bar.
- [ ] Contraste: texto ≥ 4.5:1; indicador da aba ativa ≥ 3:1 contra o strip.
- [ ] Nenhum literal de spacing/raio em chrome (`DynamicSpacing`, `HeaderBar`, `pane_corner_radius`).
- [ ] Todas as ilhas (incluindo a sidebar de threads) usam `workspace_card` e o mesmo gap.
- [ ] Temas de terceiros continuam legíveis (`TabStyle::Pill` só usa chaves existentes com fallback).

## 8. Decisões em aberto

1. `TabStyle::Pill` como padrão de todos os strips de ilha (e atualizar `crates/ui/.rules`).
2. Status bar global, fora da coluna do editor.
3. Browser como terceira aba do painel de Devices.
4. Atalhos propostos: `⌘R` Run, `⌥L` localização, `⌘K` teclado do device, `⌘↵` formatar JSON, `⌥⇧J` JSON Prettier.
5. Fonte de UI: o design usou Inter (Bear Sans UI não existe no Penpot); no app continua `ui_font_family`.
6. Raio da ilha: 12px (design) vs 8px (`pane_corner_radius()` atual).
