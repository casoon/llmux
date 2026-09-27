import { ansiToHtml } from '@casoon/pages-theme/ansi';
import type { ShowcaseExample } from '@casoon/pages-theme/showcase';

// Requests and the responses llmux returned for them, captured by examples/capture.sh
// against `llmux --demo` (built-in echo provider, no keys, no network).
const files = import.meta.glob<string>('../../examples/*.{json,headers,out}', {
  query: '?raw',
  import: 'default',
  eager: true,
});

const read = (name: string) => files[`../../examples/${name}`];

/** The request as an HTTP message: extra headers from <slug>.headers, then the JSON body. */
function request(slug: string): string {
  const headers = ['POST /v1/chat/completions', 'content-type: application/json'];
  const extra = read(`${slug}.headers`);
  if (extra) headers.push(...extra.trim().split('\n'));
  return `${headers.join('\n')}\n\n${read(`${slug}.json`).trim()}`;
}

const examples_ = [
  {
    slug: 'simple-question',
    title: 'Simple question → tier 1',
    tags: ['simple_text', 'tier 1'],
    description:
      'No keyword matches, so the prompt is simple_text with min_tier 1. The client asked for gpt-4o; llmux ignores that and picks the cheapest tier-1 model.',
  },
  {
    slug: 'code-review',
    title: 'Code review → tier 3',
    tags: ['code_review', 'tier 3'],
    description:
      '"Review this" and "function" classify the prompt as code_review, whose quality floor is tier 3. The cheapest model at or above that floor wins.',
  },
  {
    slug: 'architecture',
    title: 'Architecture question → tier 4',
    tags: ['architecture', 'tier 4'],
    description:
      '"Architecture" and "trade-offs" put the prompt into the architecture class. It needs tier 4, so it never lands on a small model.',
  },
  {
    slug: 'private-key',
    title: 'Secret in the prompt → local only',
    tags: ['privacy', 'private_sensitive', 'local_only'],
    description:
      'The prompt matches a block_cloud_patterns entry. The task becomes private_sensitive and only providers marked local: true stay eligible.',
  },
  {
    slug: 'cache-hit',
    title: 'Repeated request → cache hit',
    tags: ['cache', 'x-llmux-cache'],
    description:
      'The same request as the first example, sent again. It is answered from the SQLite exact-match cache and marked with x-llmux-cache: hit.',
  },
  {
    slug: 'forced-model',
    title: 'Forced model via header',
    tags: ['override', 'x-llmux-model'],
    description:
      'x-llmux-model skips cheapest-viable selection and pins the request to a catalog model, still subject to provider, capability, privacy and budget checks.',
  },
];

export const examples: ShowcaseExample[] = examples_.map((meta) => ({
  ...meta,
  file: `examples/${meta.slug}.json`,
  input: { code: request(meta.slug), lang: 'http' },
  output: { html: ansiToHtml(read(`${meta.slug}.out`)), kind: 'terminal' },
}));
