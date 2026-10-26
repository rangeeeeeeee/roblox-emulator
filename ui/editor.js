import {EditorState} from '@codemirror/state';
import {EditorView, keymap, lineNumbers, highlightActiveLineGutter, highlightSpecialChars,
  drawSelection, dropCursor, highlightActiveLine, hoverTooltip} from '@codemirror/view';
import {defaultKeymap, history, historyKeymap, indentWithTab, indentSelection} from '@codemirror/commands';
import {StreamLanguage, indentUnit, indentOnInput, bracketMatching, syntaxHighlighting,
  HighlightStyle, syntaxTree} from '@codemirror/language';
import {autocompletion, closeBrackets, closeBracketsKeymap, completionKeymap, snippetCompletion} from '@codemirror/autocomplete';
import {linter, lintGutter, lintKeymap, setDiagnostics, openLintPanel} from '@codemirror/lint';
import {lua} from '@codemirror/legacy-modes/mode/lua';
import {tags} from '@lezer/highlight';

export const MAX_SOURCE_BYTES = 256 * 1024;
const encoder = new TextEncoder();
export const sourceBytes = source => encoder.encode(source).length;
const luauKeywords = new Set(['continue', 'type', 'export', 'declare']);
const normalToken = lua.startState().cur;
const luau = StreamLanguage.define({
  ...lua,
  name: 'luau',
  startState: () => ({...lua.startState(), backtick: false}),
  token(stream, state) {
    if (state.backtick || (state.cur === normalToken && stream.peek() === '`')) {
      if (!state.backtick) { stream.next(); state.backtick = true; }
      let escaped = false, character;
      while ((character = stream.next()) !== undefined) {
        if (character === '`' && !escaped) { state.backtick = false; break; }
        escaped = character === '\\' && !escaped;
      }
      return 'string';
    }
    const style = lua.token(stream, state);
    return style === 'variable' && luauKeywords.has(stream.current()) ? 'keyword' : style;
  },
  languageData: {...lua.languageData, indentOnInput: /^\s*(?:end|until|else|elseif|\)|\})$/,
    closeBrackets: {brackets: ['(', '[', '{', "'", '"', '`']}}
});

const entry = (label, detail, info, type = 'function') => ({label, detail, info, type});
const globals = [
  entry('print', '(...any) → ()', 'Print values to the output.'),
  entry('warn', '(...any) → ()', 'Print a warning to the output.'),
  entry('assert', '(value, message?) → value', 'Raise an error when value is false or nil.'),
  entry('error', '(message, level?) → never', 'Raise a Luau error.'),
  entry('pcall', '(function, ...any) → (boolean, ...any)', 'Call a function and capture its error.'),
  entry('xpcall', '(function, handler, ...any)', 'Call a function with an error handler.'),
  entry('type', '(value) → string', 'Return the Luau value type.'),
  entry('typeof', '(value) → string', 'Return the Roblox value type.'),
  entry('tostring', '(value) → string', 'Convert a value to text; may call __tostring.'),
  entry('tonumber', '(value, base?) → number?', 'Convert text to a number.'),
  entry('pairs', '(table) → iterator', 'Iterate table keys and values.'),
  entry('ipairs', '(table) → iterator', 'Iterate sequential numeric entries.'),
  entry('next', '(table, key?) → (key?, value?)', 'Read the next raw table entry.'),
  entry('require', '(ModuleScript) → any', 'Load the module in the current VM.'),
  entry('select', '(index, ...any) → ...any', 'Select variadic arguments.'),
  entry('rawget', '(table, key) → any', 'Read a field without __index.'),
  entry('rawset', '(table, key, value) → table', 'Write a field without __newindex.'),
  entry('rawequal', '(a, b) → boolean', 'Compare without __eq.'),
  entry('setmetatable', '(table, metatable?) → table', 'Set a table metatable.'),
  entry('getmetatable', '(value) → any', 'Read the metatable or its protected value.'),
  ...['game', 'workspace', 'script', 'shared', '_G', 'Enum', 'Instance', 'Vector2', 'Vector3', 'CFrame', 'Color3',
    'UDim', 'UDim2', 'Ray', 'task', 'math', 'string', 'table', 'buffer', 'coroutine', 'debug', 'os', 'utf8']
    .map(label => entry(label, 'global', label, 'variable'))
];
const executor = [
  entry('getgc', '(includeTables?) → {any}', 'Snapshot GC objects in the selected VM.'),
  entry('filtergc', '(selector, options?, returnOne?) → any', 'Find GC objects matching the selector and filters.'),
  entry('getgenv', '() → table', 'Return the executor global environment.'),
  entry('getrenv', '() → table', 'Return the game environment of this VM.'),
  entry('identifyexecutor', '() → (string, string)', 'Return the active executor profile.'),
  entry('hookfunction', '(target, replacement) → function', 'Replace a function and return the original.'),
  entry('restorefunction', '(function) → ()', 'Restore a hooked function.'),
  entry('newcclosure', '(function) → function', 'Wrap a function as a C closure.'),
  entry('checkcaller', '() → boolean', 'Test the current executor origin.'),
  entry('getconnections', '(signal) → table', 'Inspect connections of a signal.'),
  entry('getinstances', '() → {Instance}', 'List Instances retained in this VM.'),
  entry('getnilinstances', '() → {Instance}', 'List unparented Instances in this VM.'),
  entry('getsenv', '(script) → table', 'Read the environment of a script.'),
  entry('getrawmetatable', '(value) → table?', 'Read the raw metatable.'),
  entry('setreadonly', '(table, readonly) → ()', 'Set the table readonly flag.'),
  entry('isreadonly', '(table) → boolean', 'Read the table readonly flag.'),
  entry('iscclosure', '(function) → boolean', 'Test whether a function is a C closure.'),
  entry('islclosure', '(function) → boolean', 'Test whether a function is a Luau closure.')
];
const members = {
  task: ['spawn', 'defer', 'delay', 'wait', 'cancel', 'synchronize', 'desynchronize'],
  table: ['insert', 'remove', 'concat', 'sort', 'find', 'clear', 'clone', 'create', 'pack', 'unpack', 'freeze', 'isfrozen'],
  string: ['format', 'sub', 'find', 'match', 'gmatch', 'gsub', 'split', 'byte', 'char', 'rep', 'len', 'lower', 'upper'],
  math: ['abs', 'floor', 'ceil', 'round', 'min', 'max', 'clamp', 'sqrt', 'random', 'sin', 'cos', 'atan2', 'pi', 'huge'],
  coroutine: ['create', 'resume', 'yield', 'status', 'running', 'wrap', 'close', 'isyieldable'],
  debug: ['info', 'traceback', 'profilebegin', 'profileend', 'setmemorycategory', 'resetmemorycategory'],
  os: ['clock', 'time', 'date', 'difftime'],
  buffer: ['create', 'fromstring', 'tostring', 'len', 'copy', 'fill', 'readu8', 'writeu8', 'readu32', 'writeu32', 'readf64', 'writef64'],
  Instance: ['new'], Vector2: ['new', 'zero', 'one'], Vector3: ['new', 'zero', 'one', 'xAxis', 'yAxis', 'zAxis'],
  CFrame: ['new', 'Angles', 'lookAt', 'fromMatrix', 'identity'], Color3: ['new', 'fromRGB', 'fromHSV', 'fromHex'],
  UDim: ['new'], UDim2: ['new', 'fromScale', 'fromOffset'], Ray: ['new']
};
const instanceMembers = ['GetService', 'FindService', 'GetChildren', 'GetDescendants', 'FindFirstChild', 'WaitForChild',
  'FindFirstChildOfClass', 'IsA', 'GetFullName', 'GetActor', 'GetAttribute', 'SetAttribute', 'GetAttributes',
  'GetPropertyChangedSignal', 'GetAttributeChangedSignal', 'Clone', 'Destroy', 'Fire', 'Invoke', 'Connect', 'Once', 'Wait',
  'Disconnect', 'BindToMessage', 'SendMessage'];
const words = ['and', 'break', 'continue', 'do', 'else', 'elseif', 'end', 'export', 'false', 'in', 'local', 'nil', 'not',
  'or', 'repeat', 'return', 'then', 'true', 'type', 'until', 'while'];
const snippets = [
  snippetCompletion('if ${condition} then\n\t${}\nend', {label: 'if', type: 'keyword', detail: 'if block'}),
  snippetCompletion('function ${name}(${args})\n\t${}\nend', {label: 'function', type: 'keyword', detail: 'function block'}),
  snippetCompletion('for ${key}, ${value} in ${iterator} do\n\t${}\nend', {label: 'for', type: 'keyword', detail: 'for block'})
];

export function createEditor(parent, options) {
  let context = options.context || 'server';
  let timer, controller, generation = 0;
  const docs = new Map([...globals, ...executor].map(item => [item.label, item]));
  function complete(request) {
    const token = request.matchBefore(/[\w.:]*/);
    if (!token || !request.explicit && !token.text) return null;
    const node = syntaxTree(request.state).resolveInner(request.pos, -1);
    if (/string|comment/i.test(node.name)) return null;
    const member = /^(.*)([.:])(\w*)$/.exec(token.text);
    if (member) {
      const name = member[1].split(/[.:]/).at(-1);
      const names = member[2] === ':' ? [...instanceMembers, ...(context === 'server' ? ['FireClient', 'FireAllClients', 'InvokeClient'] : ['FireServer', 'InvokeServer'])] : (members[name] || []);
      return {from: token.to - member[3].length, options: names.map(label => entry(label, member[2] === ':' ? 'method' : name, label)), validFor: /^\w*$/};
    }
    const locals = [...request.state.doc.toString().matchAll(/\blocal\s+(?:function\s+)?([A-Za-z_]\w*)/g)]
      .map(match => entry(match[1], 'local', 'Local declaration', 'variable'));
    const list = [...globals, ...locals, ...(context === 'executor' ? executor : []), ...words.map(label => ({label, type: 'keyword'})), ...snippets];
    return {from: token.from, options: [...new Map(list.map(item => [item.label, item])).values()], validFor: /^\w*$/};
  }
  const hover = hoverTooltip((view, position) => {
    const line = view.state.doc.lineAt(position), offset = position - line.from;
    const before = /[\w]+$/.exec(line.text.slice(0, offset))?.[0] || '';
    const after = /^\w*/.exec(line.text.slice(offset))[0];
    const word = before + after, info = docs.get(word);
    if (!info || context !== 'executor' && executor.some(item => item.label === word)) return null;
    return {pos: position - before.length, end: position + after.length, above: true, create() {
      const dom = document.createElement('div'); dom.className = 'editor-doc-tooltip';
      const signature = document.createElement('strong'); signature.textContent = info.label + ' ' + info.detail;
      const description = document.createElement('div'); description.textContent = info.info;
      dom.append(signature, description); return {dom};
    }};
  });
  function scheduleLint() {
    generation++; clearTimeout(timer); controller?.abort();
    options.onLintStatus?.('Checking…');
    timer = setTimeout(runLint, 450);
  }
  async function runLint() {
    const version = generation, document = view.state.doc, source = document.toString(), requestedContext = context;
    const token = options.getToken?.();
    if (!token) { options.onLintStatus?.('Lint offline'); return; }
    if (sourceBytes(source) > MAX_SOURCE_BYTES) {
      view.dispatch(setDiagnostics(view.state, [{from: 0, to: 0, severity: 'error', message: 'Source exceeds 256 KiB.'}]));
      options.onLintStatus?.('Source too large'); return;
    }
    controller = new AbortController();
    const activeController = controller, signal = activeController.signal;
    const timeout = setTimeout(() => activeController.abort(), 10000);
    try {
      const response = await fetch('/api/lint', {method: 'POST', credentials: 'same-origin', cache: 'no-store', signal,
        headers: {'Content-Type': 'application/json', 'X-Emulator-Token': token}, body: JSON.stringify({source, context: requestedContext})});
      const result = await response.json();
      if (!response.ok) throw new Error(typeof result.error === 'string' ? result.error : 'HTTP ' + response.status);
      if (version !== generation || document !== view.state.doc || requestedContext !== context) return;
      if (!Array.isArray(result.diagnostics)) throw new Error('Invalid lint response');
      const offset = point => {
        const line = document.line(Math.max(1, Math.min(document.lines, (point?.line || 0) + 1)));
        return line.from + Math.max(0, Math.min(line.length, point?.column || 0));
      };
      const diagnostics = result.diagnostics.map(item => {
        const from = offset(item.from), to = Math.max(from, offset(item.to));
        return {from, to, severity: item.severity === 'warning' ? 'warning' : 'error', message: String(item.message), source: item.code ? String(item.code) : 'Luau'};
      });
      view.dispatch(setDiagnostics(view.state, diagnostics));
      const errors = diagnostics.filter(item => item.severity === 'error').length;
      options.onLintStatus?.(diagnostics.length ? `${errors} errors · ${diagnostics.length - errors} warnings` : 'No issues');
    } catch (error) {
      if (version !== generation) return;
      view.dispatch(setDiagnostics(view.state, []));
      options.onLintStatus?.('Lint unavailable', error.name === 'AbortError' ? 'Lint request timed out' : error.message);
    } finally { clearTimeout(timeout); }
  }
  const extensions = [
    lineNumbers(), highlightActiveLineGutter(), highlightSpecialChars(), history(), drawSelection(), dropCursor(),
    EditorState.allowMultipleSelections.of(true), indentUnit.of('    '), luau, indentOnInput(), bracketMatching(),
    closeBrackets(), autocompletion({override: [complete]}), highlightActiveLine(), hover,
    linter(null), lintGutter(),
    keymap.of([{key: 'Mod-Enter', run: () => { options.onRun?.(); return true; }},
      ...closeBracketsKeymap, ...defaultKeymap, ...historyKeymap, ...completionKeymap, ...lintKeymap, indentWithTab,
      {key: 'Escape', run: () => { parent.dispatchEvent(new CustomEvent('editor-escape', {bubbles: true})); return true; }}]),
    EditorView.contentAttributes.of({'aria-label': 'Luau source', spellcheck: 'false', autocapitalize: 'off'}),
    EditorState.transactionFilter.of(transaction => {
      if (transaction.docChanged && sourceBytes(transaction.newDoc.toString()) > MAX_SOURCE_BYTES) {
        options.onSizeError?.('Source exceeds 256 KiB.'); return [];
      }
      return transaction;
    }),
    EditorView.updateListener.of(update => {
      if (update.docChanged) { options.onChange?.(update.state.doc.toString()); scheduleLint(); }
      if (update.docChanged || update.selectionSet) options.onCursor?.();
    }),
    syntaxHighlighting(HighlightStyle.define([
      {tag: tags.keyword, color: '#c5a4f5'}, {tag: [tags.string, tags.special(tags.string)], color: '#b5d69d'},
      {tag: tags.number, color: '#edbf8f'}, {tag: tags.comment, color: '#76879b', fontStyle: 'italic'},
      {tag: [tags.name, tags.variableName], color: '#dce8f7'}, {tag: tags.standard(tags.name), color: '#84c7ee'},
      {tag: tags.operator, color: '#d5b7ee'}, {tag: tags.invalid, color: '#ff929a'}
    ])),
    EditorView.theme({
      '&': {height: '100%', backgroundColor: '#11161d', color: '#dce8f7'},
      '.cm-scroller': {overflow: 'auto', fontFamily: 'ui-monospace, "Cascadia Code", Consolas, monospace', fontSize: '14px', lineHeight: '1.65'},
      '.cm-content': {padding: '14px 0', caretColor: '#e7edf5'}, '.cm-line': {padding: '0 12px'},
      '.cm-gutters': {backgroundColor: '#11161d', color: '#65778d', borderRight: '1px solid #27313e'},
      '.cm-activeLineGutter, .cm-activeLine': {backgroundColor: '#1a2532'},
      '&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection': {backgroundColor: '#3c5472'},
      '&.cm-focused': {outline: 'none'}, '.cm-cursor, .cm-dropCursor': {borderLeftColor: '#e7edf5'},
      '.cm-tooltip': {backgroundColor: '#202a36', border: '1px solid #4c5e73', color: '#e7edf5', borderRadius: '6px', maxWidth: 'min(560px, 85vw)'},
      '.cm-tooltip-autocomplete > ul > li[aria-selected]': {backgroundColor: '#344e6d', color: '#fff'},
      '.cm-panels': {backgroundColor: '#1b2430', color: '#e7edf5'},
      '.cm-panel.cm-panel-lint button': {minHeight: '20px'},
      '.cm-matchingBracket': {backgroundColor: '#3a5365', outline: '1px solid #6d8aa6'},
      '.cm-foldPlaceholder': {backgroundColor: '#263343', border: 'none', color: '#aebfd4'}
    }, {dark: true})
  ];
  const view = new EditorView({parent, state: EditorState.create({doc: options.doc || '', extensions})});
  const contextStates = new Map();
  scheduleLint();
  return {
    getValue: () => view.state.doc.toString(),
    setValue(value) {
      if (sourceBytes(value) > MAX_SOURCE_BYTES) { options.onSizeError?.('Source exceeds 256 KiB.'); return false; }
      view.dispatch({changes: {from: 0, to: view.state.doc.length, insert: value}, selection: {anchor: 0}}); return true;
    },
    setContext(value) {
      if (value === context) return;
      contextStates.set(context, view.state); context = value;
      view.setState(contextStates.get(value) || EditorState.create({extensions})); scheduleLint();
    },
    getSelection: () => view.state.selection.main,
    position() { const head = view.state.selection.main.head, line = view.state.doc.lineAt(head); return {line: line.number, column: head - line.from + 1}; },
    insert(value) { view.dispatch(view.state.replaceSelection(value)); view.focus(); },
    focus: () => view.focus(),
    indent: () => indentSelection(view),
    showDiagnostics: () => openLintPanel(view),
    destroy() { clearTimeout(timer); controller?.abort(); view.destroy(); }
  };
}
