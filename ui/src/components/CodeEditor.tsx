import { autocompletion, closeBrackets, closeBracketsKeymap, completionKeymap } from '@codemirror/autocomplete';
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';
import { json, jsonParseLinter } from '@codemirror/lang-json';
import { xml } from '@codemirror/lang-xml';
import { yaml } from '@codemirror/lang-yaml';
import {
  bracketMatching,
  foldGutter,
  foldKeymap,
  HighlightStyle,
  indentOnInput,
  syntaxHighlighting,
} from '@codemirror/language';
import { linter, lintGutter, lintKeymap, type Diagnostic } from '@codemirror/lint';
import { highlightSelectionMatches, searchKeymap } from '@codemirror/search';
import { Compartment, EditorState, type Extension } from '@codemirror/state';
import {
  drawSelection,
  EditorView,
  highlightActiveLine,
  highlightActiveLineGutter,
  keymap,
  lineNumbers,
  placeholder as cmPlaceholder,
} from '@codemirror/view';
import { tags } from '@lezer/highlight';
import { useComputedColorScheme } from '@mantine/core';
import { useEffect, useRef } from 'react';
import { schemaCompletion, schemaDiagnostics, schemaHover } from '../lib/cmJsonSchema';
import { styleNonce } from '../lib/nonce';
import type { SchemaContext } from '../lib/jsonSchema';

export type CodeLanguage = 'json' | 'yaml' | 'xml' | 'text';

export interface CodeEditorProps {
  value: string;
  onChange?: (value: string) => void;
  language?: CodeLanguage;
  readOnly?: boolean;
  schema?: SchemaContext | null;
  minHeight?: number;
  maxHeight?: number;
  placeholder?: string;
  label?: string;
  onDiagnostics?: (count: number) => void;
  testId?: string;
}

const lightHighlight = HighlightStyle.define([
  { tag: tags.propertyName, color: '#364fc7' },
  { tag: tags.string, color: '#2b8a3e' },
  { tag: [tags.number, tags.bool, tags.null], color: '#c2255c' },
  { tag: [tags.tagName], color: '#1864ab' },
  { tag: [tags.attributeName], color: '#e8590c' },
  { tag: tags.comment, color: '#868e96', fontStyle: 'italic' },
  { tag: tags.keyword, color: '#862e9c' },
  { tag: [tags.punctuation, tags.bracket], color: '#495057' },
]);

const darkHighlight = HighlightStyle.define([
  { tag: tags.propertyName, color: '#91a7ff' },
  { tag: tags.string, color: '#8ce99a' },
  { tag: [tags.number, tags.bool, tags.null], color: '#faa2c1' },
  { tag: [tags.tagName], color: '#74c0fc' },
  { tag: [tags.attributeName], color: '#ffc078' },
  { tag: tags.comment, color: '#909296', fontStyle: 'italic' },
  { tag: tags.keyword, color: '#e599f7' },
  { tag: [tags.punctuation, tags.bracket], color: '#c1c2c5' },
]);

function baseTheme(dark: boolean): Extension {
  return EditorView.theme(
    {
      '&': {
        backgroundColor: 'var(--fm-code-bg)',
        color: 'var(--mantine-color-text)',
      },
      '.cm-scroller': { fontFamily: 'var(--mantine-font-family-monospace)', lineHeight: '1.5' },
      '.cm-content': { caretColor: 'var(--mantine-color-text)' },
      '.cm-gutters': {
        backgroundColor: 'var(--fm-code-bg)',
        color: 'var(--mantine-color-dimmed)',
        borderRight: '1px solid var(--mantine-color-default-border)',
      },
      '.cm-activeLine': { backgroundColor: dark ? 'rgba(255,255,255,0.04)' : 'rgba(0,0,0,0.035)' },
      '.cm-activeLineGutter': { backgroundColor: dark ? 'rgba(255,255,255,0.06)' : 'rgba(0,0,0,0.05)' },
      '&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection': {
        backgroundColor: dark ? 'rgba(116,143,252,0.35)' : 'rgba(76,110,245,0.22)',
      },
      '.cm-tooltip': {
        backgroundColor: 'var(--mantine-color-body)',
        color: 'var(--mantine-color-text)',
        border: '1px solid var(--mantine-color-default-border)',
        borderRadius: '6px',
      },
      '.cm-tooltip-autocomplete > ul > li[aria-selected]': {
        backgroundColor: 'var(--mantine-primary-color-filled)',
        color: 'var(--mantine-color-white)',
      },
      '.cm-schema-tooltip': { padding: '6px 8px', maxWidth: '420px', fontSize: '0.8rem' },
      '.cm-diagnostic': { fontSize: '0.8rem' },
      '.cm-placeholder': { color: 'var(--mantine-color-dimmed)' },
    },
    { dark },
  );
}

function languageExtension(lang: CodeLanguage): Extension {
  switch (lang) {
    case 'json':
      return json();
    case 'yaml':
      return yaml();
    case 'xml':
      return xml();
    default:
      return [];
  }
}

export function CodeEditor({
  value,
  onChange,
  language = 'json',
  readOnly = false,
  schema = null,
  minHeight = 160,
  maxHeight = 560,
  placeholder,
  label,
  onDiagnostics,
  testId,
}: CodeEditorProps) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const themeComp = useRef(new Compartment());
  const roComp = useRef(new Compartment());
  const onChangeRef = useRef(onChange);
  const onDiagRef = useRef(onDiagnostics);
  const schemaRef = useRef<SchemaContext | null>(schema);
  const dark = useComputedColorScheme('light') === 'dark';

  useEffect(() => {
    onChangeRef.current = onChange;
    onDiagRef.current = onDiagnostics;
    schemaRef.current = schema;
  });

  useEffect(() => {
    if (!host.current) return;
    const getSchema = () => schemaRef.current;
    const nonce = styleNonce();
    const extensions: Extension[] = [
      ...(nonce ? [EditorView.cspNonce.of(nonce)] : []),
      lineNumbers(),
      highlightActiveLineGutter(),
      foldGutter(),
      history(),
      drawSelection(),
      indentOnInput(),
      bracketMatching(),
      closeBrackets(),
      highlightActiveLine(),
      highlightSelectionMatches(),
      EditorView.lineWrapping,
      keymap.of([...closeBracketsKeymap, ...defaultKeymap, ...searchKeymap, ...historyKeymap, ...foldKeymap, ...completionKeymap, ...lintKeymap]),
      languageExtension(language),
      themeComp.current.of([baseTheme(dark), syntaxHighlighting(dark ? darkHighlight : lightHighlight)]),
      roComp.current.of([EditorState.readOnly.of(readOnly), EditorView.editable.of(!readOnly)]),
      EditorView.theme({
        '&': { minHeight: `${minHeight}px`, maxHeight: `${maxHeight}px` },
        '.cm-scroller': { overflow: 'auto', minHeight: `${minHeight}px`, maxHeight: `${maxHeight}px` },
      }),
      EditorView.contentAttributes.of({
        'aria-label': label ?? 'Code editor',
        ...(testId ? { 'data-testid': testId } : {}),
      }),
      EditorView.updateListener.of((u) => {
        if (u.docChanged) onChangeRef.current?.(u.state.doc.toString());
      }),
    ];
    if (placeholder) extensions.push(cmPlaceholder(placeholder));
    if (language === 'json' && !readOnly) {
      const parse = jsonParseLinter();
      extensions.push(
        lintGutter(),
        linter(
          (v): Diagnostic[] => {
            const text = v.state.doc.toString();
            const diags = text.trim() ? parse(v) : [];
            const all = diags.length ? diags : (() => {
              const ctx = getSchema();
              return ctx ? schemaDiagnostics(v.state, ctx) : [];
            })();
            onDiagRef.current?.(all.length);
            return all;
          },
          { delay: 300 },
        ),
        autocompletion({ override: [schemaCompletion(getSchema)], activateOnTyping: true }),
        schemaHover(getSchema),
      );
    } else if (!readOnly) {
      extensions.push(autocompletion());
    }
    const v = new EditorView({
      state: EditorState.create({ doc: value, extensions }),
      parent: host.current,
    });
    view.current = v;
    return () => {
      v.destroy();
      view.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [language, minHeight, maxHeight, placeholder, label, testId]);

  useEffect(() => {
    view.current?.dispatch({
      effects: themeComp.current.reconfigure([baseTheme(dark), syntaxHighlighting(dark ? darkHighlight : lightHighlight)]),
    });
  }, [dark]);

  useEffect(() => {
    view.current?.dispatch({
      effects: roComp.current.reconfigure([EditorState.readOnly.of(readOnly), EditorView.editable.of(!readOnly)]),
    });
  }, [readOnly]);

  useEffect(() => {
    const v = view.current;
    if (!v) return;
    const current = v.state.doc.toString();
    if (current !== value) {
      v.dispatch({ changes: { from: 0, to: current.length, insert: value } });
    }
  }, [value]);

  return <div className="cm-host" ref={host} />;
}
