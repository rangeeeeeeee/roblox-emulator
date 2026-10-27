import {EditorState, Prec} from '@codemirror/state';
import {EditorView, keymap, lineNumbers, highlightActiveLineGutter, highlightSpecialChars,
  drawSelection, dropCursor, highlightActiveLine, hoverTooltip} from '@codemirror/view';
import {defaultKeymap, history, historyKeymap, indentWithTab, indentSelection, isolateHistory} from '@codemirror/commands';
import {StreamLanguage, indentUnit, indentOnInput, bracketMatching, syntaxHighlighting,
  HighlightStyle, syntaxTree} from '@codemirror/language';
import {autocompletion, closeBrackets, closeBracketsKeymap, completionKeymap, snippetCompletion, acceptCompletion, closeCompletion} from '@codemirror/autocomplete';
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
    'UDim', 'UDim2', 'Random', 'Ray', 'Rect', 'NumberRange', 'NumberSequence', 'NumberSequenceKeypoint',
    'ColorSequence', 'ColorSequenceKeypoint', 'TweenInfo', 'Path2DControlPoint', 'Vector2int16', 'Vector3int16',
    'RaycastParams', 'OverlapParams', 'task', 'math', 'string', 'table', 'buffer', 'coroutine', 'debug', 'os', 'utf8']
    .map(label => entry(label, 'global', label, 'variable'))
];
const executor = [
  {...entry('GCViewer', 'Instance.new("GCViewer")', 'Open the executor-only GC viewer window.', 'class'), apply: 'Instance.new("GCViewer")'},
  entry('getgc', '(includeTables?) → {any}', 'Snapshot GC objects in the selected VM.'),
  entry('filtergc', '(selector, options?, returnOne?) → any', 'Find GC objects matching the selector and filters.'),
  entry('getupvalue', '(functionOrLevel, index) → any', 'Read a captured upvalue. Same native function as debug.getupvalue; C closures are unsupported.'),
  entry('getupvalues', '(functionOrLevel) → table', 'Read captured upvalues at their original indices. Same native function as debug.getupvalues.'),
  entry('setupvalue', '(functionOrLevel, index, value) → ()', 'Change a captured upvalue. Same native function as debug.setupvalue.'),
  entry('getconstant', '(functionOrLevel, index) → value?', 'Read a Luau constant at its original index; hidden or out-of-range slots return nil.'),
  entry('getconstants', '(functionOrLevel) → table', 'Read the Luau constant pool, preserving indices and nil gaps. C closures are unsupported.'),
  entry('setconstant', '(functionOrLevel, index, value) → ()', 'Change a primitive constant. This emulator preserves its existing type; other type changes report UnsupportedApi.'),
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
  UDim: ['new'], UDim2: ['new', 'fromScale', 'fromOffset'], Random: ['new'], Ray: ['new'],
  Rect: ['new'], NumberRange: ['new'], NumberSequence: ['new'], NumberSequenceKeypoint: ['new'],
  ColorSequence: ['new'], ColorSequenceKeypoint: ['new'], TweenInfo: ['new'], Path2DControlPoint: ['new'],
  Vector2int16: ['new'], Vector3int16: ['new'], RaycastParams: ['new'], OverlapParams: ['new']
};
const instanceMembers = ['GetService', 'FindService', 'GetChildren', 'GetDescendants', 'FindFirstChild', 'WaitForChild',
  'FindFirstChildOfClass', 'IsA', 'GetFullName', 'GetActor', 'GetAttribute', 'SetAttribute', 'GetAttributes',
  'GetPropertyChangedSignal', 'GetAttributeChangedSignal', 'Clone', 'Destroy', 'Fire', 'Invoke', 'Connect', 'Once', 'Wait',
  'Disconnect', 'BindToMessage', 'SendMessage'];
const words = ['and', 'break', 'continue', 'do', 'else', 'elseif', 'end', 'export', 'false', 'in', 'local', 'nil', 'not',
  'or', 'repeat', 'return', 'then', 'true', 'type', 'until', 'while'];
const reservedWords = new Set([...words, 'if', 'for', 'function']);
const snippets = [
  snippetCompletion('if ${condition} then\n\t${}\nend', {label: 'if', type: 'keyword', detail: 'if block'}),
  snippetCompletion('function ${name}(${args})\n\t${}\nend', {label: 'function', type: 'keyword', detail: 'function block'}),
  snippetCompletion('for ${key}, ${value} in ${iterator} do\n\t${}\nend', {label: 'for', type: 'keyword', detail: 'for block'})
];

// Resolve literal Instance paths only. Hints never evaluate Luau or invoke Instance methods.
function pathTokens(source) {
  const pattern = /--\[(=*)\[[\s\S]*?\]\1\]|--[^\r\n]*|"(?:\\[\s\S]|[^"\\])*"|'(?:\\[\s\S]|[^'\\])*'|`(?:\\[\s\S]|[^`\\])*`|\[(=*)\[[\s\S]*?\]\2\]|[A-Za-z_]\w*|[^\s]/g;
  return [...source.matchAll(pattern)].filter(match => !match[0].startsWith('--')).map(match => ({text: match[0], from: match.index, to: match.index + match[0].length}));
}
function literalString(token) {
  const text = token?.text || '';
  const long = /^\[(=*)\[([\s\S]*)\]\1\]$/.exec(text);
  if (long) return long[2].replace(/^\r?\n/, '');
  if (!/^(["'])[\s\S]*\1$/.test(text)) return null;
  let valid = true;
  const escaped = {a: '\x07', b: '\b', f: '\f', n: '\n', r: '\r', t: '\t', v: '\v', '\\': '\\', '"': '"', "'": "'"};
  const value = text.slice(1, -1).replace(/\\(x[\da-fA-F]{2}|u\{[\da-fA-F]+\}|\d{1,3}|z\s*|\r?\n|[\s\S])/g, (_, escape) => {
    if (Object.hasOwn(escaped, escape)) return escaped[escape];
    if (escape[0] === 'z') return '';
    if (escape[0] === 'x') return String.fromCharCode(parseInt(escape.slice(1), 16));
    if (escape[0] === 'u') {
      const point = parseInt(escape.slice(2, -1), 16);
      if (point <= 0x10ffff) return String.fromCodePoint(point);
    } else if (/^\d+$/.test(escape) && Number(escape) <= 255) return String.fromCharCode(Number(escape));
    else if (/\r?\n/.test(escape)) return '\n';
    valid = false; return '';
  });
  return valid ? value : null;
}
function readPath(tokens, start, aliases) {
  const root = tokens[start]?.text;
  let path = root === 'game' || root === 'workspace' ? [root] : aliases.get(root)?.slice();
  if (!path) return null;
  let index = start + 1;
  while (index < tokens.length && path.length < 32) {
    const token = tokens[index].text;
    if (token === '.' && /^[A-Za-z_]\w*$/.test(tokens[index + 1]?.text || '')) {
      path.push(tokens[index + 1].text); index += 2;
    } else if (token === '[' && tokens[index + 2]?.text === ']') {
      const name = literalString(tokens[index + 1]); if (name === null) break;
      path.push(name); index += 3;
    } else if (token === ':' && ['GetService', 'FindService', 'WaitForChild', 'FindFirstChild'].includes(tokens[index + 1]?.text) && tokens[index + 2]?.text === '(') {
      const method = tokens[index + 1].text, name = literalString(tokens[index + 3]);
      if (name === null || tokens[index + 4]?.text !== ')') break;
      if ((method === 'GetService' || method === 'FindService') && (path.length !== 1 || path[0] !== 'game')) break;
      path.push(name); index += 5;
    } else break;
  }
  return {path, end: index};
}
function instancePath(source) {
  const tokens = pathTokens(source), aliases = new Map();
  for (let index = 0; index < tokens.length; index++) {
    const local = tokens[index].text === 'local';
    const nameIndex = local ? index + 1 : index, name = tokens[nameIndex]?.text;
    if (!/^[A-Za-z_]\w*$/.test(name || '') || !local && !aliases.has(name)) continue;
    if (!local && index > 0 && tokens[index - 1].text !== ';' && !/[\r\n]/.test(source.slice(tokens[index - 1].to, tokens[index].from))) continue;
    let equals = nameIndex + 1;
    if (tokens[equals]?.text === ':') while (equals < tokens.length && tokens[equals].text !== '=' && tokens[equals].text !== ';') equals++;
    if (tokens[equals]?.text !== '=') { if (local) aliases.delete(name); continue; }
    const result = readPath(tokens, equals + 1, aliases);
    const next = result && tokens[result.end];
    const boundary = result && (!next || [';', 'end', 'else', 'elseif', 'until'].includes(next.text) || /[\r\n]/.test(source.slice(tokens[result.end - 1].to, next.from)));
    if (result && boundary) aliases.set(name, result.path);
    else aliases.delete(name);
  }
  for (let index = Math.max(0, tokens.length - 160); index < tokens.length; index++) {
    const result = readPath(tokens, index, aliases);
    if (result?.end === tokens.length) return result.path;
  }
  return null;
}

// A small lexical pass keeps block editing independent of lint/network results.
// Strings (including long brackets/interpolation) and comments are never scanned as code.
function blockTokens(source) {
  const tokens = [];
  for (let at = 0; at < source.length;) {
    if (/\s/.test(source[at])) { at++; continue; }
    const from = at, comment = source.startsWith('--', at);
    if (comment) at += 2;
    const long = /^\[(=*)\[/.exec(source.slice(at));
    if (long) {
      const close = ']' + long[1] + ']', end = source.indexOf(close, at + long[0].length);
      at = end < 0 ? source.length : end + close.length;
      tokens.push({text: source.slice(from, at), from, to: at, kind: comment ? 'comment' : 'literal', open: end < 0}); continue;
    }
    if (comment) {
      at = source.indexOf('\n', at); if (at < 0) at = source.length;
      tokens.push({text: source.slice(from, at), from, to: at, kind: 'comment', open: true}); continue;
    }
    const quote = source[at];
    if (quote === '"' || quote === "'" || quote === '`') {
      let closed = false; at++;
      while (at < source.length) {
        if (source[at] === '\\') { at = Math.min(source.length, at + 2); continue; }
        if (source[at++] === quote) { closed = true; break; }
      }
      tokens.push({text: source.slice(from, at), from, to: at, kind: 'literal', open: !closed}); continue;
    }
    const word = /^[A-Za-z_]\w*/.exec(source.slice(at));
    const number = /^(?:0[xX][\da-fA-F]+|\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)/.exec(source.slice(at));
    const symbol = /^(?:\.\.\.|\.\.|->|::|==|~=|<=|>=)/.exec(source.slice(at));
    const text = word?.[0] || number?.[0] || symbol?.[0] || source[at++];
    if (word || number || symbol) at += text.length;
    tokens.push({text, from, to: at, kind: word ? 'word' : number ? 'number' : 'symbol'});
  }
  return tokens;
}
function balanced(tokens, angles = false) {
  const stack = [], pairs = {')': '(', ']': '[', '}': '{', '>': '<'};
  for (const {text, kind} of tokens) {
    if (kind === 'literal') continue;
    if (['(', '[', '{', ...(angles ? ['<'] : [])].includes(text)) stack.push(text);
    else if (Object.hasOwn(pairs, text) && (angles || text !== '>') && stack.pop() !== pairs[text]) return false;
  }
  return stack.length === 0;
}
function completeExpression(tokens) {
  const last = tokens.at(-1);
  return !!last && balanced(tokens) && !tokens.some(token => token.open)
    && !tokens.some(token => token.kind === 'word' && ['function', 'if', 'then', 'else', 'elseif', 'end', 'do', 'for', 'while', 'repeat', 'until', 'return', 'local', 'break', 'continue'].includes(token.text))
    && (last.kind === 'literal' || last.kind === 'number' || [')', ']', '}', '...'].includes(last.text)
      || last.kind === 'word' && (!reservedWords.has(last.text) || ['true', 'false', 'nil'].includes(last.text)));
}
function functionHeader(tokens) {
  let index = 1;
  if (tokens[index]?.kind === 'word') {
    index++;
    while (['.', ':'].includes(tokens[index]?.text) && tokens[index + 1]?.kind === 'word') index += 2;
  }
  if (tokens[index]?.text === '<') {
    let depth = 1; index++;
    while (index < tokens.length && depth) { if (tokens[index].text === '<') depth++; if (tokens[index].text === '>') depth--; index++; }
    if (depth) return false;
  }
  if (tokens[index]?.text !== '(') return false;
  const start = index++; let depth = 1;
  while (index < tokens.length && depth) { if (tokens[index].text === '(') depth++; if (tokens[index].text === ')') depth--; index++; }
  if (depth || !balanced(tokens.slice(start, index))) return false;
  if (index === tokens.length) return true;
  if (tokens[index++].text !== ':') return false;
  const type = tokens.slice(index);
  const last = type.at(-1);
  if (!last || !balanced(type, true) || !(last.kind !== 'symbol' || [')', ']', '}', '>', '?', '...'].includes(last.text))) return false;
  return type.every((token, i) => token.kind === 'literal' || token.kind === 'number'
    || token.kind === 'word' && !['return', 'local', 'end', 'do', 'then', 'else', 'for', 'while', 'if', 'break', 'continue', 'function'].includes(token.text)
      && type[i - 1]?.kind !== 'word'
    || token.kind === 'symbol' && ['.', ':', '?', '|', '&', '->', '...', ',', '(', ')', '{', '}', '[', ']', '<', '>'].includes(token.text));
}
function closingBlock(tokens, start, afterHeader, source) {
  const stack = [{kind: tokens[start].text}];
  const indentation = token => /^[\t ]*/.exec(source.slice(source.lastIndexOf('\n', token.from - 1) + 1))[0];
  const base = indentation(tokens[start]);
  for (let index = afterHeader; index < tokens.length; index++) {
    const token = tokens[index]; if (token.kind !== 'word') continue;
    const word = token.text, previous = tokens[index - 1];
    if (word === 'function' || word === 'for' || word === 'while' || word === 'repeat'
      || word === 'if' && (/[\r\n]/.test(source.slice(previous.to, token.from)) && !['=', 'return', ',', '('].includes(previous.text)
        || ['then', 'do', 'else', ';'].includes(previous.text))) stack.push({kind: word, pending: ['for', 'while'].includes(word)});
    else if (word === 'do') { if (stack.at(-1)?.pending) stack.at(-1).pending = false; else stack.push({kind: word}); }
    else if (word === 'end' || word === 'until') {
      // An aligned closer also covers Luau if-expressions, whose nested `if`
      // keywords do not introduce statement blocks or require another `end`.
      if ((word === 'until') === (tokens[start].text === 'repeat') && indentation(token) === base) return token;
      if ((word === 'until') !== (stack.at(-1)?.kind === 'repeat')) return null;
      stack.pop(); if (!stack.length) return token;
    }
  }
  return null;
}

export function insertBlock(view) {
  const {state} = view, selection = state.selection.main;
  if (view.composing || !selection.empty || state.selection.ranges.length !== 1) return false;
  const cursor = selection.head, line = state.doc.lineAt(cursor), source = state.doc.toString(), all = blockTokens(source);
  if (all.some(token => ['literal', 'comment'].includes(token.kind) && token.from < cursor && (cursor < token.to || cursor === token.to && token.open))) return false;
  const tokens = all.filter(token => token.kind !== 'comment'), before = tokens.filter(token => token.to <= cursor);
  if (!before.length || state.doc.lineAt(before.at(-1).to).number !== line.number) return false;
  const first = before.findIndex(token => token.from >= line.from);
  let start = first, kind = before[start]?.text, suffix = '';
  if (!['for', 'while', 'if', 'elseif', 'else', 'do', 'repeat'].includes(kind)) {
    start = before.findLastIndex(token => token.text === 'function' && token.kind === 'word'); kind = 'function';
    if (start < 0 || cursor - before[start].from > 8192 || !functionHeader(before.slice(start))) return false;
  } else {
    const header = before.slice(start + 1);
    if (['else', 'do', 'repeat'].includes(kind)) { if (header.length) return false; }
    else {
      const ending = kind === 'if' || kind === 'elseif' ? 'then' : 'do';
      if (header.at(-1)?.text === ending) header.pop(); else suffix = ' ' + ending;
      if (kind === 'for') {
        let split = 0;
        while (header[split]?.kind === 'word' && !reservedWords.has(header[split].text)) { split++; if (header[split]?.text !== ',') break; split++; }
        if (!/^[A-Za-z_]\w*(?:,[A-Za-z_]\w*)*$/.test(header.slice(0, split).map(token => token.text).join(''))
          || !['in', '='].includes(header[split]?.text) || !completeExpression(header.slice(split + 1))) return false;
        if (header[split].text === '=') {
          let depth = 0, comma = false;
          for (const token of header.slice(split + 1)) { if (['(', '[', '{'].includes(token.text)) depth++; if ([')', ']', '}'].includes(token.text)) depth--; if (token.text === ',' && !depth) comma = true; }
          if (!comma || split !== 1) return false;
        }
      } else if (!completeExpression(header)) return false;
    }
  }
  const after = source.slice(cursor, line.to);
  if (!/^\s*(?:(?:end|until)\b)?[\s)\]},;]*$/.test(after)) return false;
  const headerLine = state.doc.lineAt(before[start].from), base = /^\s*/.exec(headerLine.text)[0];
  const inner = base + state.facet(indentUnit), close = closingBlock(tokens, start, before.length, source);
  const existing = close && (state.doc.lineAt(close.from).number === line.number || /^\s*/.exec(state.doc.lineAt(close.from).text)[0].length >= base.length);
  const branch = kind === 'else' || kind === 'elseif';
  let insert = suffix + '\n' + inner, to = cursor + /^\s*/.exec(after)[0].length;
  const anchor = cursor + insert.length;
  if (!branch && !existing) insert += '\n' + base + (kind === 'repeat' ? 'until ' : 'end');
  else if (existing && close.from >= cursor && close.from <= line.to && !source.slice(cursor, close.from).trim()) insert += '\n' + base;
  view.dispatch({changes: {from: cursor, to, insert}, selection: {anchor}, scrollIntoView: true,
    userEvent: 'input', annotations: isolateHistory.of('full')});
  return true;
}

export function createEditor(parent, options) {
  let context = options.context || 'server';
  let timer, controller, generation = 0;
  const docs = new Map([...globals, ...executor].map(item => [item.label, item]));
  function complete(request) {
    const token = request.matchBefore(/\w*/);
    if (!token) return null;
    const node = syntaxTree(request.state).resolveInner(request.pos, -1);
    if (/string|comment/i.test(node.name)) return null;
    const before = request.state.doc.sliceString(0, token.from).trimEnd();
    const operator = before.at(-1);
    if (!request.explicit && !token.text && operator !== '.' && operator !== ':') return null;
    if (operator === '.' || operator === ':') {
      const receiver = before.slice(0, -1), path = instancePath(receiver);
      const name = /([A-Za-z_]\w*)\s*$/.exec(receiver)?.[1];
      const names = operator === ':' ? [...instanceMembers, ...(context === 'server' ? ['FireClient', 'FireAllClients', 'InvokeClient'] : ['FireServer', 'InvokeServer'])]
        : [...(members[name] || (path ? ['Name', 'ClassName', 'Parent', 'Archivable', ...instanceMembers] : [])),
          ...(name === 'debug' && context === 'executor' ? ['getupvalue', 'getupvalues', 'setupvalue', 'getconstant', 'getconstants', 'setconstant'] : [])];
      const fallback = {from: token.from, options: names.map(label => name === 'debug' && context === 'executor' && docs.has(label)
        ? docs.get(label) : entry(label, operator === ':' ? 'method' : name || 'member', label)), validFor: /^\w*$/};
      return path ? completeInstance(request, path, operator, before.length - 1, fallback) : fallback;
    }
    const locals = [...request.state.doc.toString().matchAll(/\blocal\s+(?:function\s+)?([A-Za-z_]\w*)/g)]
      .map(match => entry(match[1], 'local', 'Local declaration', 'variable'));
    const list = [...globals, ...locals, ...(context === 'executor' ? executor : []), ...words.map(label => ({label, type: 'keyword'})), ...snippets];
    return {from: token.from, options: [...new Map(list.map(item => [item.label, item])).values()], validFor: /^\w*$/};
  }
  async function completeInstance(request, path, operator, operatorPosition, fallback) {
    const realm = options.getRealm?.(), requestedContext = context, token = options.getToken?.();
    if (!realm || !token) return fallback;
    const abort = new AbortController();
    request.addEventListener('abort', () => abort.abort(), {onDocChange: true});
    const timeout = setTimeout(() => abort.abort(), 4000);
    try {
      const response = await fetch('/api/completions', {method: 'POST', credentials: 'same-origin', cache: 'no-store', signal: abort.signal,
        headers: {'Content-Type': 'application/json', 'X-Emulator-Token': token}, body: JSON.stringify({realm, context: requestedContext, path})});
      const result = await response.json();
      if (request.aborted || context !== requestedContext || options.getRealm?.() !== realm) return null;
      if (!response.ok || !Array.isArray(result.items) || !result.items.length) return fallback;
      const items = result.items.filter(item => typeof item.label === 'string' && (operator !== ':' || item.type === 'method' || item.type === 'function')).map(item => {
        const completion = entry(item.label, String(item.detail || ''), String(item.info || ''), typeof item.type === 'string' ? item.type : 'property');
        if (operator === '.' && (!/^[A-Za-z_]\w*$/.test(item.label) || reservedWords.has(item.label))) {
          const quoted = JSON.stringify(item.label).replace(/\\u([\da-fA-F]{4})/g, '\\u{$1}');
          completion.apply = (view, _completion, _from, to) => view.dispatch({changes: {from: operatorPosition, to, insert: '[' + quoted + ']'}, selection: {anchor: operatorPosition + quoted.length + 2}, scrollIntoView: true});
        }
        return completion;
      });
      return {...fallback, options: items};
    } catch { return request.aborted || context !== requestedContext || options.getRealm?.() !== realm ? null : fallback; }
    finally { clearTimeout(timeout); }
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
    Prec.highest(keymap.of([{key: 'Tab', run: acceptCompletion}, {key: 'Enter', run: insertBlock}])),
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
      '.cm-activeLineGutter': {backgroundColor: '#1a2532'}, '.cm-activeLine': {backgroundColor: 'transparent'},
      '.cm-selectionBackground': {backgroundColor: '#375678'},
      '&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground': {backgroundColor: '#436f9d'},
      '.cm-content ::selection': {backgroundColor: '#436f9d'},
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
    refreshCompletions: () => closeCompletion(view),
    destroy() { clearTimeout(timer); controller?.abort(); view.destroy(); }
  };
}
