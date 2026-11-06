# Benchmarks dos modelos

O Automático lê a tabela e as regras abaixo para escolher o modelo e o effort de cada subagent.
Edite à vontade: o padrão do Asylum continua guardado, e "Restaurar padrão" volta para ele.

revisado: 2026-11-06

## Modelos

Um modelo pode ter uma linha por effort, porque o score muda com o nível.
- `id`: provedor/modelo; vale para o mesmo modelo em qualquer provedor (Anthropic, assinatura, Bedrock…).
- `effort`: low, medium, high, xhigh ou max.
- `tier`: alto, médio ou baixo.
- `terminal_bench` e `frontiercode`: % publicado pelo laboratório; — quando não há número para aquele effort.
- `custo`: preço por token relativo à Sonnet 5.5 (= 1.0). Um effort maior gasta mais tokens por tarefa.

| id | effort | tier | terminal_bench | frontiercode | preço in/out | custo | bom para | evitar para | fonte | data |
|---|---|---|---|---|---|---|---|---|---|---|
| anthropic/claude-haiku-5-5 | medium | baixo | — | — | $0.10 / $0.50 | 0.05 | busca, leitura e resumo curtos | qualquer edição de código | anthropic.com/claude-haiku-5-5 | 2026-10-07 |
| anthropic/claude-haiku-5-5 | max | baixo | 39.2 | 46.4 | $0.10 / $0.50 | 0.05 | edição mecânica com plano pronto, resumo longo | regra de negócio, depuração | anthropic.com/claude-haiku-5-5 | 2026-10-07 |
| anthropic/claude-sonnet-5-5 | medium | médio | — | — | $2 / $10 | 1.0 | testes e ajustes com plano pronto | arquitetura nova | anthropic.com/claude-sonnet-5-5 | 2026-09-28 |
| anthropic/claude-sonnet-5-5 | high | médio | 70.6 | — | $2 / $10 | 1.0 | implementação com escopo fechado, scripts, terminal | arquitetura nova | anthropic.com/claude-sonnet-5-5 | 2026-09-28 |
| anthropic/claude-sonnet-5-5 | xhigh | médio | — | 52.1 | $2 / $10 | 1.0 | implementação que mexe em vários arquivos | busca simples | anthropic.com/claude-sonnet-5-5 | 2026-09-28 |
| anthropic/claude-opus-5-5 | medium | alto | — | 54.6 | $4 / $20 | 2.0 | refatoração em vários arquivos | busca simples | anthropic.com/claude-opus-5-5 | 2026-09-22 |
| anthropic/claude-opus-5-5 | xhigh | alto | 66.4 | — | $4 / $20 | 2.0 | depuração difícil, mudanças grandes | busca simples | anthropic.com/claude-opus-5-5 | 2026-09-22 |
| anthropic/claude-fable-5-1 | high | alto | 55.8 | — | $10 / $50 | 5.0 | só quando o usuário pedir | subagent em geral | anthropic.com/claude-fable-and-mythos-5-1 | 2026-09-15 |

## Regras

- Escolha o modelo e o effort mais baratos que terminam a subtarefa sem retrabalho.
- Busca, leitura e resumo vão para o tier baixo; código com regra de negócio, não.
- Implementação com escopo fechado vai para o tier médio; depuração difícil e mudanças grandes, para o alto.
- Suba o effort antes de trocar de tier; xhigh só para depuração e mudanças grandes.
- Na dúvida entre duas linhas, fique com a mais forte: refazer custa mais que a diferença de preço.
- Respeite a coluna "evitar para".

## Fontes

- anthropic.com: páginas de lançamento do Opus 5.5, Sonnet 5.5, Haiku 5.5 e Fable 5.1 (setembro e outubro de 2026).
- O Terminal-Bench 4.0 da Sonnet 5.5 não informa o effort; está no nível padrão da plataforma (high).
- O Terminal-Bench 4.0 da Opus 5.5 é em xhigh; o FrontierCode em medium vem do texto da página.
- A Anthropic recomenda a Haiku 5.5 como subagent de trabalho de código com escopo estreito.
