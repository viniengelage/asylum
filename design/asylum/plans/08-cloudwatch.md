# 08 · CloudWatch (Logs Insights + Live tail + Agent)

**Boards:** `10 · CloudWatch` → "Workspace — CloudWatch (Insights)", "(live tail)", "(sessão expirada)",
"(pedir query ao Agent)", "(resumo do Agent)", "(conversa com o Agent)", "(corrigir consulta)" e
"Specs — CloudWatch". O alvo é 1:1 com esses boards.

## Contexto

O CloudWatch foi desenhado em 2026-09-28, ganhou um cliente (47adb8d65f0) e saiu do código quando o
time passou para o Elasticsearch (4852e8ea81). Em 2026-11-17 o usuário pediu a integração de volta,
fiel ao Pencil. O `crates/elastic` é o molde: dock, aba de consulta, seguir, toolkit e cards.

## Decisões

- SDK oficial (`aws-sdk-cloudwatchlogs =1.117.0`, mesmo train dos `aws-smithy-*` do lock) sobre o
  `AwsHttpClient` do Zed, rodando no `gpui_tokio`. Credenciais pela cadeia padrão do `aws-config`
  (`aws login`, SSO, `credential_process`, chaves, env); feature `credentials-login` ligada no workspace.
  Nenhuma chave é colada no Asylum.
- Perfil e região por projeto no KVP do perfil do Asylum. Log groups do projeto em
  `.asylum/cloudwatch/groups.json` e queries em `.asylum/cloudwatch/queries/*.insights` (vão no git).
- Toolkit `cloudwatch` só de leitura. Até 1 h de janela roda sem pedir; acima disso, `ToolAccess::Act`
  com a janela e os grupos no prompt.
- Textos por `i18n::t!`, um namespace por frente (`cloudwatch_panel`, `cloudwatch_query`,
  `cloudwatch_tail`, `cloudwatch_agent`) para as fases andarem em paralelo.

## Arquitetura (`crates/cloudwatch`)

| Módulo | O quê |
|---|---|
| `client.rs`, `profiles.rs` | SDK: conexão, log groups, Insights (Start/GetQueryResults/StopQuery), FilterLogEvents, StartLiveTail; `~/.aws/config` |
| `store.rs` | `CloudWatchStore` por workspace: alvo (perfil/região), conexão, sessão expirada, log groups, grupos do projeto, queries salvas, uso do dia |
| `panel.rs` | Dock: cabeçalho do perfil, região, "só leitura", filtro, DO PROJETO, TODOS por prefixo, QUERIES SALVAS, "Hoje neste perfil", Live tail ⌘⇧L, sessão; estado expirado com PERFIS e PERMISSÕES |
| `query_view.rs` (+ submódulos) | Aba `.insights`: barra (grupos, período, limite, Pedir ao Agent ⌘K, Histórico, Executar ⌘↵), editor, validação, resultados (Eventos/Gráfico/Campos, histograma, evento aberto com stack trace em link e ações), rodapé de exportação; cards de erro com correção e referência rápida, sessão expirada com `aws login` no terminal, ⌘K com diff, resumo do Agent |
| `live_tail_view.rs` | Aba de live tail: filtro, pausar ⌘., taxa, sessão cobrada, busca, seguir o fim, limites, fecha após 10 min fora de foco, "Mandar para o Agent", "Salvar como .log", detector de padrão novo (local, 24 h) com aviso escrito pelo modelo |
| `agent_toolkit.rs` | Tools `cloudwatch_list_log_groups`, `cloudwatch_insights_query`, `cloudwatch_wide_insights_query`, `cloudwatch_recent_events` |
| `agent_ui/.../cloudwatch_card.rs` | Card "Consultou o CloudWatch" no thread; menção `@` de grupo, consulta e evento |

## Fases

0. **Base** — crate restaurado, SDK no workspace, `store.rs`. Compila. (Teste contra conta real:
   `cargo test -p cloudwatch -- --ignored`, no terminal do usuário.)
1. **Dock** — `panel.rs` + botão no toolkit da status bar + id de toolkit.
2. **Insights** — aba `.insights`, resultados, histograma, queries salvas, histórico, erro com correção,
   sessão expirada, ⌘K com diff, resumo.
3. **Live tail** — aba, filtro, pausar, limites, padrão novo.
4. **Agent** — toolkit, card, menções, "Perguntar ao Agent" a partir de um evento.

## Verificação

`cargo check --workspace --all-targets`, `./script/clippy`, testes de parser/estado (stack trace,
validação, detector de padrão, groups.json) e o app aberto com o dock. Consulta real só com a conta do
usuário.
