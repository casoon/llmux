export type RouteStatus = 'allowed' | 'degraded' | 'cached' | 'fallback' | 'rejected' | 'forced';

export const overview = {
  status: 'Proxy online',
  requestsPerMinute: 42,
  todayCost: 1.27,
  budgetUsed: 64,
  cacheHitRate: 31,
  p95Latency: 1840,
};

export const routeFeed = [
  {
    time: '17:42:18',
    tool: 'codex',
    project: 'llmux',
    task: 'code_review',
    model: 'gpt-4.1-mini',
    tier: 3,
    cost: '$0.0021',
    latency: '1.3s',
    status: 'allowed' as RouteStatus,
    chips: ['tools', 'cloud'],
    reason: 'Tier floor 3 met, tool support required, cheapest viable paid model.',
  },
  {
    time: '17:42:03',
    tool: 'aider',
    project: 'client-api',
    task: 'private_sensitive',
    model: 'qwen2.5',
    tier: 1,
    cost: '$0.0000',
    latency: '0.8s',
    status: 'forced' as RouteStatus,
    chips: ['local', 'privacy'],
    reason: 'Secret pattern matched. Cloud models excluded by policy.',
  },
  {
    time: '17:41:47',
    tool: 'continue',
    project: 'docs',
    task: 'summarize',
    model: 'llama-3.1-8b:free',
    tier: 1,
    cost: '$0.0000',
    latency: '2.1s',
    status: 'cached' as RouteStatus,
    chips: ['cache', 'cloud'],
    reason: 'Exact request matched cached response for the selected model.',
  },
  {
    time: '17:41:12',
    tool: 'agent',
    project: 'checkout',
    task: 'architecture',
    model: 'gemini-pro-1.5',
    tier: 4,
    cost: '$0.0184',
    latency: '4.7s',
    status: 'fallback' as RouteStatus,
    chips: ['fallback', 'large-context'],
    reason: 'Primary provider returned 429; fallback preserved tier and context fit.',
  },
  {
    time: '17:40:58',
    tool: 'codex',
    project: 'llmux',
    task: 'code_review',
    model: 'gemini-flash-1.5',
    tier: 2,
    cost: '$0.0012',
    latency: '0.9s',
    status: 'degraded' as RouteStatus,
    chips: ['budget', 'cloud'],
    reason: 'Daily budget pressure capped max tier at 2.',
  },
  {
    time: '17:40:31',
    tool: 'custom',
    project: 'finance',
    task: 'private_sensitive',
    model: '-',
    tier: 0,
    cost: '$0.0000',
    latency: '0.0s',
    status: 'rejected' as RouteStatus,
    chips: ['policy', 'override'],
    reason: 'Forced cloud override rejected for local-only project policy.',
  },
];

export const policySignals = [
  { label: 'Allowed', value: 1248, tone: 'success', trend: '+8%' },
  { label: 'Rejected', value: 17, tone: 'error', trend: '-3%' },
  { label: 'Degraded', value: 86, tone: 'warning', trend: '+12%' },
  { label: 'Forced', value: 41, tone: 'info', trend: '+2%' },
  { label: 'Local only', value: 132, tone: 'success', trend: '+18%' },
];

export const modelMatrix = [
  { model: 'qwen2.5', provider: 'ollama', tier: 1, cost: '$0', p50: '0.8s', p95: '1.9s', success: '96%', caps: ['local', '32k'] },
  { model: 'llama-3.1-8b:free', provider: 'openrouter', tier: 1, cost: '$0', p50: '1.9s', p95: '4.1s', success: '92%', caps: ['tools', '128k'] },
  { model: 'gemini-flash-1.5', provider: 'openrouter', tier: 2, cost: '$', p50: '0.7s', p95: '1.5s', success: '94%', caps: ['tools', 'json', '1m'] },
  { model: 'gpt-4.1-mini', provider: 'openai', tier: 3, cost: '$$', p50: '1.2s', p95: '2.8s', success: '98%', caps: ['tools', 'json', '128k'] },
  { model: 'claude-3.5-sonnet', provider: 'openrouter', tier: 4, cost: '$$$', p50: '2.6s', p95: '6.4s', success: '97%', caps: ['tools', 'reasoning', '200k'] },
];

export const projectRows = [
  { name: 'llmux', spend: '$0.42', requests: 384, local: '28%', rejects: 3 },
  { name: 'client-api', spend: '$0.18', requests: 129, local: '71%', rejects: 1 },
  { name: 'checkout', spend: '$0.51', requests: 211, local: '12%', rejects: 8 },
  { name: 'docs', spend: '$0.04', requests: 492, local: '6%', rejects: 0 },
];

export const budgetSeries = [18, 22, 31, 38, 43, 45, 52, 58, 64, 66, 64, 68];
