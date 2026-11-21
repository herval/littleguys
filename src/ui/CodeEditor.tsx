// A text editor for files in a guy's folder (CodeMirror 6), dressed like the read-only
// source view beside it. Loaded only when you edit a file (see Files.tsx), so it costs
// nothing until then; each language loads on its own the first time it's needed.
import { indentWithTab } from "@codemirror/commands";
import { HighlightStyle, LanguageDescription, syntaxHighlighting } from "@codemirror/language";
import { languages } from "@codemirror/language-data";
import { Compartment, EditorState } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { tags as t } from "@lezer/highlight";
import { basicSetup } from "codemirror";
import { useEffect, useRef } from "react";

// The source view's colors (styles.css, `.code-view` and `.hljs-*`).
const theme = EditorView.theme(
  {
    "&": { height: "100%", backgroundColor: "#201c1a", color: "#f3f0ee", fontSize: "12.5px" },
    "&.cm-focused": { outline: "none" },
    ".cm-scroller": { fontFamily: "var(--font-mono)", lineHeight: "1.6", padding: "12px 0" },
    ".cm-content": { caretColor: "#ff7a45", paddingRight: "16px" },
    ".cm-cursor, .cm-dropCursor": { borderLeftColor: "#ff7a45", borderLeftWidth: "2px" },
    "&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-selectionBackground, ::selection": {
      backgroundColor: "rgb(255 122 69 / 0.28) !important",
    },
    ".cm-activeLine": { backgroundColor: "rgb(255 255 255 / 0.035)" },
    ".cm-gutters": { backgroundColor: "#201c1a", color: "rgb(243 240 238 / 0.28)", border: "none", paddingLeft: "8px" },
    ".cm-activeLineGutter": { backgroundColor: "transparent", color: "rgb(243 240 238 / 0.6)" },
    ".cm-foldPlaceholder": { backgroundColor: "rgb(255 255 255 / 0.08)", border: "none", color: "#bdb5af" },
    ".cm-matchingBracket": { backgroundColor: "rgb(255 255 255 / 0.1)", outline: "none" },
    ".cm-searchMatch": { backgroundColor: "rgb(248 197 85 / 0.25)" },
    ".cm-searchMatch.cm-searchMatch-selected": { backgroundColor: "rgb(248 197 85 / 0.45)" },
    ".cm-selectionMatch": { backgroundColor: "rgb(255 255 255 / 0.08)" },
    ".cm-panels": { backgroundColor: "#2a2523", color: "#f3f0ee", borderColor: "rgb(255 255 255 / 0.09)" },
    ".cm-panel input, .cm-panel button": { fontSize: "12px" },
    ".cm-tooltip": { backgroundColor: "#2a2523", color: "#f3f0ee", border: "1px solid rgb(255 255 255 / 0.09)" },
  },
  { dark: true },
);

const highlight = HighlightStyle.define([
  { tag: [t.keyword, t.operatorKeyword, t.modifier, t.controlKeyword, t.definitionKeyword, t.moduleKeyword], color: "#ff9e7a" },
  { tag: [t.string, t.special(t.string), t.regexp, t.attributeName, t.attributeValue], color: "#b5e18b" },
  { tag: [t.number, t.bool, t.null, t.atom], color: "#f8c555" },
  { tag: [t.comment, t.lineComment, t.blockComment, t.docComment], color: "#8a817c", fontStyle: "italic" },
  { tag: [t.function(t.variableName), t.function(t.propertyName), t.typeName, t.className, t.tagName, t.heading], color: "#8cc8ff" },
  { tag: t.heading, fontWeight: "600" },
  { tag: [t.link, t.url], color: "#8cc8ff", textDecoration: "underline" },
  { tag: t.emphasis, fontStyle: "italic" },
  { tag: t.strong, fontWeight: "600" },
  { tag: [t.meta, t.processingInstruction], color: "#bdb5af" },
  { tag: t.invalid, color: "#ff453a" },
]);

export interface CodeEditorProps {
  value: string;
  /** Picks the language, by extension or name (`Makefile`). */
  filename: string;
  onChange(value: string): void;
  /** ⌘S / Ctrl+S. */
  onSave(): void;
  /** Esc. */
  onCancel(): void;
}

export default function CodeEditor({ value, filename, onChange, onSave, onCancel }: CodeEditorProps) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  // The callbacks change every render; the editor keeps calling the latest ones.
  const handlers = useRef({ onChange, onSave, onCancel });
  handlers.current = { onChange, onSave, onCancel };

  useEffect(() => {
    const language = new Compartment();
    const editor = new EditorView({
      parent: host.current!,
      state: EditorState.create({
        doc: value,
        extensions: [
          keymap.of([{ key: "Mod-s", preventDefault: true, run: () => (handlers.current.onSave(), true) }]),
          basicSetup,
          // After basicSetup's, so Esc closes a completion list before it leaves the editor.
          keymap.of([indentWithTab, { key: "Escape", run: () => (handlers.current.onCancel(), true) }]),
          language.of([]),
          theme,
          syntaxHighlighting(highlight),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) handlers.current.onChange(u.state.doc.toString());
          }),
        ],
      }),
    });
    view.current = editor;
    editor.focus();
    const found = LanguageDescription.matchFilename(languages, filename);
    let gone = false;
    found
      ?.load()
      .then((support) => !gone && editor.dispatch({ effects: language.reconfigure(support) }))
      .catch(() => {});
    return () => {
      gone = true;
      editor.destroy();
      view.current = null;
    };
    // One editor per file: Files.tsx mounts a new one for another file.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filename]);

  // Text replaced from outside (their version reloaded): show it.
  useEffect(() => {
    const editor = view.current;
    if (editor && editor.state.doc.toString() !== value) {
      editor.dispatch({ changes: { from: 0, to: editor.state.doc.length, insert: value } });
    }
  }, [value]);

  return <div ref={host} className="h-full min-h-0 overflow-hidden" />;
}
