import type { ThemeRegistration } from 'shiki';

// Derived from the prototype's hand-coloured config window (`.syntax-*`) so
// fenced blocks, generated signatures and the landing visuals share one look.
const base = '#c9e0d7';
const key = '#85dce0';
const string = '#b7ea89';
const punctuation = '#dae8d7';
const comment = '#668086';
const bright = '#f0f7f3';
const constant = '#d2dfa2';
const keyword = '#a9f36d';

export const devsandboxesTheme: ThemeRegistration = {
  name: 'devsandboxes',
  type: 'dark',
  colors: {
    'editor.background': '#111b1e',
    'editor.foreground': base,
  },
  fg: base,
  bg: '#111b1e',
  settings: [
    { settings: { foreground: base, background: '#111b1e' } },
    {
      scope: ['comment', 'punctuation.definition.comment'],
      settings: { foreground: comment, fontStyle: 'italic' },
    },
    {
      scope: [
        'punctuation',
        'meta.brace',
        'punctuation.definition.table',
        'punctuation.definition.array',
        'punctuation.separator',
        'keyword.operator',
      ],
      settings: { foreground: punctuation },
    },
    {
      scope: [
        'support.type.property-name',
        'entity.name.tag',
        'variable.other.property',
        'variable.other.object.property',
        'meta.object-literal.key',
        'variable.parameter',
        'entity.other.attribute-name',
        'keyword.key.toml',
        'variable.other.key.toml',
        'support.type.property-name.toml',
        'entity.name.section.toml',
        'entity.other.attribute-name.table.toml',
      ],
      settings: { foreground: key },
    },
    {
      scope: [
        'string',
        'string.quoted',
        'punctuation.definition.string',
        'string.template',
        'markup.inline.raw',
      ],
      settings: { foreground: string },
    },
    {
      scope: [
        'constant.numeric',
        'constant.language',
        'constant.other',
        'constant.character',
        'support.constant',
      ],
      settings: { foreground: constant },
    },
    {
      scope: ['keyword', 'storage', 'storage.type', 'storage.modifier', 'keyword.control'],
      settings: { foreground: keyword },
    },
    {
      scope: [
        'entity.name.function',
        'support.function',
        'entity.name.command',
        'support.function.builtin',
        'meta.function-call',
      ],
      settings: { foreground: bright },
    },
    {
      scope: ['entity.name.type', 'support.type', 'support.class', 'entity.name.class'],
      settings: { foreground: '#91e6e8' },
    },
    {
      scope: ['variable.other.normal', 'variable.other.bracket', 'variable.other.special'],
      settings: { foreground: key },
    },
  ],
};
