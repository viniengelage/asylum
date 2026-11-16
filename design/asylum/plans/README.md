# Planos de implementação (design → código)

Cada arquivo aqui é **um** item desenhado no `design/asylum/openpencil/Asylum.fig`, pronto para virar
código. A ideia é atacar um de cada vez, na ordem abaixo, e marcar o status aqui quando terminar.

| # | Plano | Boards | Tamanho | Depende de | Status |
|---|---|---|---|---|---|
| 1 | [Servidores MCP vira atalho](01-mcp-menu-atalho.md) | 19 · Projetos | P | — | fase 1 feita (63060b2c59); fase 2 (linhas da página de MCP) a fazer |
| 2 | [Contexto da thread](02-contexto-da-thread.md) | 14 · Notas | M | — | fase 1 feita (a4857e2d82, editor abre como modal, não popover ancorado); fases 2–3 a fazer |
| 3 | [Projetos](03-projetos.md) | 19 · Projetos | G | — | a fazer |
| 4 | [Lançamentos (ASC + Play + EAS)](04-lancamentos.md) | 18 · Lançamentos | G | 3 (fase 3) para contas por projeto | a fazer |
| 5 | [Executar e depurar (tasks + debugger)](05-executar-e-depurar.md) | 20 · Executar e depurar | G | — (fase 0 é independente e pequena) | fase 0 feita (2c9f9d0770, 09f23ec86d), menos aspas nos args; fases 1–4 a fazer |
| 6 | [Imagens nas notas](06-imagens-nas-notas.md) | 14 · Notas | M | — (combina com 02, que também mexe em notas) | @nota com imagens feito (aad1535a6a); colar em .md já existia no editor; resto a fazer |
| 7 | [Build limpa a partir de uma branch (EAS)](07-build-limpa-por-branch.md) | 18 · Lançamentos | M | 05 (fase 2) e 04 (fase 1) | a fazer |

**Por que essa ordem:** 1 e 2 são pequenos, independentes e resolvem dores do dia a dia (MCP caindo
no lugar errado, se perder nas threads). 3 muda onde as configs e as contas moram, então vem antes de
4: Lançamentos começa lendo contas do perfil e ganha "só este projeto" quando Projetos chegar na fase 3.

## Como executar um plano

1. Abrir o board citado no plano e exportar o PNG para comparar lado a lado (skill `asylum-flow-design`
   / `asylum-ui-impl`).
2. Antes de mexer em código: as duas linhas `> [!IMPORTANT]` no topo do `README.md` (regra do CLAUDE.md).
3. Seguir as fases do plano; cada fase termina compilando, com `./script/clippy` limpo nos crates tocados
   e testes do que tem lógica (merge de configs, parsers, polling).
4. Textos novos via `i18n::t!` (en + pt-BR), nunca literal.
5. Validar no app (perfil real) antes de marcar como feito; anotar aqui o commit.

## Itens que ainda vão entrar

Novos desenhos ganham o próximo número (`05-…`). Se um desenho mexer num plano existente, atualizar
o plano em vez de criar outro.
