# Planos de implementação (design → código)

Cada arquivo aqui é **um** item desenhado no `design/asylum/openpencil/Asylum.fig`, pronto para virar
código. A ideia é atacar um de cada vez, na ordem abaixo, e marcar o status aqui quando terminar.

| # | Plano | Boards | Tamanho | Depende de | Status |
|---|---|---|---|---|---|
| 1 | [Servidores MCP vira atalho](01-mcp-menu-atalho.md) | 19 · Projetos | P | — | fase 1 (63060b2c59) e fase 2 (e3b506854d) feitas; não validado no app |
| 2 | [Contexto da thread](02-contexto-da-thread.md) | 14 · Notas | M | — | fases 1–3 feitas (a4857e2d82, f1985fa07d, 9df4696992, 5e6745f4aa); não validado no app |
| 3 | [Projetos](03-projetos.md) | 19 · Projetos | G | — | fase 0 feita; fases 1, 2 e 4 parciais (713e0049dd, cd9c3bccca, 1a0b869649); fase 3 coberta pelo plano 04 (contas no perfil); ver pendências no plano |
| 4 | [Lançamentos (ASC + Play + EAS)](04-lancamentos.md) | 18 · Lançamentos | G | 3 (fase 3) para contas por projeto | fases 1–4 feitas (37a61a936f…34247ad9cb), só testadas com fixtures; contas no perfil até o 03 fase 3 |
| 5 | [Executar e depurar (tasks + debugger)](05-executar-e-depurar.md) | 20 · Executar e depurar | G | — (fase 0 é independente e pequena) | fases 0–4 feitas (2c9f9d0770, 09f23ec86d, 2031b17745…a652c7c70e); não validado no app |
| 6 | [Imagens nas notas](06-imagens-nas-notas.md) | 14 · Notas | M | — (combina com 02, que também mexe em notas) | fases 1–4 feitas em boa parte (aad1535a6a, dd17b31b15, 5e6745f4aa); pendências no plano |
| 7 | [Build limpa a partir de uma branch (EAS)](07-build-limpa-por-branch.md) | 18 · Lançamentos | M | 05 (fase 2) e 04 (fase 1) | fases 1–3 feitas (7624225b8b e seguinte); falta a tool do agente |
| 8 | [CloudWatch (Insights + Live tail + Agent)](08-cloudwatch.md) | 10 · CloudWatch | G | — | fases 0–4 feitas (878cb4afb9 e seguinte); compila, clippy e testes ok; não validado no app nem contra a conta AWS |
| 9 | [Cenas (layouts nomeados, colunas e linhas)](09-cenas.md) | 21 · Cenas | G | — (base `unified_panes` já existe) | fases 0 e 1 feitas (revisão da base de slots incluída); fases 2–4 pendentes; não validado no app |

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
