// @ts-check
import casoonPages from '@casoon/pages-theme';
import { defineConfig } from 'astro/config';

// Project page: https://casoon.github.io/llmux/ — `base` is the GitHub Pages path.
export default defineConfig({
  site: 'https://casoon.github.io/llmux',
  base: '/llmux/',
  integrations: [
    casoonPages({
      name: 'llmux',
      description:
        'Intent-based local LLM router: an OpenAI-compatible proxy that classifies each prompt and picks the cheapest viable model, provider and cost tier.',
      repo: 'casoon/llmux',
      version: '0.1.0',
      license: 'Apache-2.0',
      // Not published to a registry yet; the crate name "llmux" on crates.io belongs to another project.
      packages: [],
      // Root-level pages in docs/ join the first group.
      docsGroups: {
        guides: 'Guides',
        reference: 'Reference',
      },
    }),
  ],
});
