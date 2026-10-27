// Structural validation and opcode normalization before passing compiler-produced
// bytecode to Luau. This is not a sandbox for hostile bytecode (nor is luau_load).
#include "Luau/Bytecode.h"
#include "Luau/BytecodeUtils.h"
#include <algorithm>
#include <cmath>
#include <cstring>
#include <stdexcept>
#include <string>
#include <vector>

namespace {
void require(bool ok, const char* message) { if (!ok) throw std::runtime_error(message); }
struct Reader {
    unsigned char* data; size_t size; size_t pos = 0;
    void skip(size_t n) { require(n <= size - pos, "Truncated bytecode"); pos += n; }
    unsigned byte() { skip(1); return data[pos - 1]; }
    uint32_t word() { skip(4); uint32_t v; memcpy(&v, data + pos - 4, 4); return v; }
    uint64_t var64() {
        uint64_t result = 0;
        for (unsigned shift = 0; shift < 64; shift += 7) {
            unsigned b = byte(); require(shift != 63 || b <= 1, "Invalid bytecode varint");
            result |= uint64_t(b & 127) << shift; if (b < 128) return result;
        }
        throw std::runtime_error("Invalid bytecode varint");
    }
    unsigned var() { auto v = var64(); require(v <= UINT32_MAX, "Invalid bytecode varint"); return unsigned(v); }
    unsigned count() { unsigned n = var(); require(n <= size, "Bytecode count exceeds input size"); return n; }
    unsigned string(unsigned strings, bool optional = true) {
        unsigned n = var(); require(n <= strings && (optional || n != 0), "Invalid bytecode string index"); return n;
    }
};
struct Proto {
    unsigned stack, params, upvalues, variadic;
    std::vector<uint32_t> code, children, closures;
    std::vector<unsigned> constants;
};
void instructions(const Proto& p, const std::vector<Proto>& protos) {
    const auto& code = p.code;
    std::vector<bool> starts(code.size());
    for (size_t pc = 0; pc < code.size();) {
        auto op = LuauOpcode(LUAU_INSN_OP(code[pc]));
        require(op < LOP__COUNT && op != LOP_NATIVECALL && op != LOP_BREAK, "Invalid or runtime-only opcode");
        starts[pc] = true; pc += Luau::getOpLength(op); require(pc <= code.size(), "Missing instruction AUX word");
    }
    auto reg = [&](unsigned r) { require(r < p.stack, "Invalid register index"); };
    auto constant = [&](unsigned k, int kind = -1) {
        require(k < p.constants.size(), "Invalid constant index");
        require(kind < 0 || p.constants[k] == unsigned(kind), "Invalid constant type");
    };
    auto import = [&](uint32_t id) {
        require(id >> 30, "Empty import path");
        for (unsigned j = 0; j < (id >> 30); j++) constant((id >> (20 - 10 * j)) & 1023, LBC_CONSTANT_STRING);
    };
    for (size_t pc = 0; pc < code.size();) {
        auto insn = code[pc]; auto op = LuauOpcode(LUAU_INSN_OP(insn));
        unsigned a = LUAU_INSN_A(insn), b = LUAU_INSN_B(insn), c = LUAU_INSN_C(insn);
        int d = LUAU_INSN_D(insn); unsigned length = Luau::getOpLength(op);
        uint32_t aux = length == 2 ? code[pc + 1] : 0;
        auto jump = [&](int offset) { int64_t dest = int64_t(pc) + 1 + offset; require(dest >= 0 && size_t(dest) < code.size() && starts[dest], "Invalid jump target"); };
        if (Luau::isJumpD(op)) jump(d);
        if (op == LOP_JUMPX) jump(LUAU_INSN_E(insn));
        switch (op) {
        case LOP_NOP: case LOP_JUMP: case LOP_JUMPBACK: case LOP_JUMPX: case LOP_COVERAGE: break;
        case LOP_PREPVARARGS: require(p.variadic && a == p.params, "Invalid vararg prologue"); break;
        case LOP_CAPTURE:
            require(a <= LCT_UPVAL, "Invalid capture kind");
            if (a == LCT_UPVAL) require(b < p.upvalues, "Invalid capture upvalue"); else reg(b); break;
        case LOP_FASTCALL: case LOP_FASTCALL1: case LOP_FASTCALL2: case LOP_FASTCALL2K: case LOP_FASTCALL3: case LOP_FASTPCALL:
            jump(c); if (op != LOP_FASTCALL && op != LOP_FASTPCALL) reg(b);
            if (op == LOP_FASTCALL2 || op == LOP_FASTCALL3) reg(aux & 255);
            if (op == LOP_FASTCALL3) reg((aux >> 8) & 255);
            if (op == LOP_FASTCALL2K) constant(aux); break;
        default: reg(a); break;
        }
        switch (op) {
        case LOP_LOADB: require(b <= 1, "Invalid boolean operand"); jump(c); break;
        case LOP_LOADK: constant(d); break;
        case LOP_LOADKX: constant(aux); break;
        case LOP_GETGLOBAL: case LOP_SETGLOBAL: constant(aux, LBC_CONSTANT_STRING); break;
        case LOP_GETUPVAL: case LOP_SETUPVAL: require(b < p.upvalues, "Invalid upvalue index"); break;
        case LOP_GETIMPORT: constant(d, LBC_CONSTANT_IMPORT); import(aux); break;
        case LOP_GETTABLEKS: case LOP_SETTABLEKS: case LOP_NAMECALL:
            reg(b); constant(aux, LBC_CONSTANT_STRING); if (op == LOP_NAMECALL) reg(a + 1); break;
        case LOP_GETUDATAKS: case LOP_SETUDATAKS: case LOP_NAMECALLUDATA:
            throw std::runtime_error("Runtime-specialized userdata opcodes are not portable; use compiler bytecode");
        case LOP_GETTABLE: case LOP_SETTABLE: case LOP_ADD: case LOP_SUB: case LOP_MUL: case LOP_DIV:
        case LOP_MOD: case LOP_POW: case LOP_IDIV: case LOP_AND: case LOP_OR: case LOP_CONCAT: reg(b); reg(c); break;
        case LOP_MOVE: case LOP_GETTABLEN: case LOP_SETTABLEN: case LOP_NOT: case LOP_MINUS: case LOP_LENGTH: reg(b); break;
        case LOP_ADDK: case LOP_SUBK: case LOP_MULK: case LOP_DIVK: case LOP_MODK: case LOP_POWK: case LOP_IDIVK:
        case LOP_ANDK: case LOP_ORK: reg(b); constant(c); break;
        case LOP_SUBRK: case LOP_DIVRK: constant(b, LBC_CONSTANT_NUMBER); reg(c); break;
        case LOP_JUMPIFEQ: case LOP_JUMPIFLE: case LOP_JUMPIFLT: case LOP_JUMPIFNOTEQ: case LOP_JUMPIFNOTLE: case LOP_JUMPIFNOTLT: reg(aux); break;
        case LOP_JUMPXEQKN: constant(aux & 0xffffff, LBC_CONSTANT_NUMBER); break;
        case LOP_JUMPXEQKS: constant(aux & 0xffffff, LBC_CONSTANT_STRING); break;
        case LOP_CALL: case LOP_CALLFB:
            if (b) reg(a + b - 1); if (c > 1) reg(a + c - 2); break;
        case LOP_RETURN: case LOP_GETVARARGS: if (b > 1) reg(a + b - 2); break;
        case LOP_FORNPREP: case LOP_FORNLOOP: reg(a + 2); break;
        case LOP_FORGPREP: case LOP_FORGPREP_INEXT: case LOP_FORGPREP_NEXT: reg(a + 3); break;
        case LOP_FORGLOOP: require((aux & 255) > 0, "Invalid iterator result count"); reg(a + 2 + (aux & 255)); break;
        case LOP_SETLIST: reg(b); if (c > 1) reg(b + c - 2); require(aux > 0, "Invalid list index"); break;
        case LOP_NEWTABLE: require(b <= 31 && aux <= 1000000, "Table allocation is too large"); break;
        case LOP_DUPTABLE: constant(d); require(p.constants[d] == LBC_CONSTANT_TABLE || p.constants[d] == LBC_CONSTANT_TABLE_WITH_CONSTANTS, "Invalid table template"); break;
        case LOP_NEWCLOSURE: case LOP_DUPCLOSURE: {
            unsigned child;
            if (op == LOP_NEWCLOSURE) { require(unsigned(d) < p.children.size(), "Invalid child prototype"); child = p.children[d]; }
            else { constant(d, LBC_CONSTANT_CLOSURE); child = p.closures[d]; }
            unsigned captures = protos[child].upvalues;
            require(captures < code.size() - pc, "Missing closure captures");
            for (unsigned i = 1; i <= captures; i++) require(LUAU_INSN_OP(code[pc + i]) == LOP_CAPTURE, "Missing closure capture");
            break;
        }
        case LOP_NEWCLASS: case LOP_NEWCLASSMEMBER: throw std::runtime_error("Class bytecode is not supported by this importer");
        default: break;
        }
        pc += length;
    }
}
size_t prepare(std::vector<unsigned char>& bytes, unsigned multiplier) {
    Reader r{bytes.data(), bytes.size()};
    unsigned version = r.byte();
    require(version >= 3 && version <= 14, "Unsupported bytecode version (importer supports Luau 3..14)");
    unsigned types = version >= 4 ? r.byte() : 0;
    require(version < 4 || (types >= 1 && types <= 3), "Unsupported bytecode type version");
    unsigned strings = r.count();
    for (unsigned i = 0; i < strings; i++) { unsigned n = r.count(); r.skip(n); }
    if (types == 3) while (r.byte()) r.string(strings, false);
    unsigned count = r.count(); require(count > 0, "Bytecode has no prototypes");
    std::vector<Proto> protos; protos.reserve(count);
    for (unsigned i = 0; i < count; i++) {
        size_t end = r.size;
        if (version >= 12) { unsigned n = r.count(); require(n <= r.size - r.pos, "Truncated prototype"); end = r.pos + n; }
        Proto p; p.stack = r.byte(); p.params = r.byte(); p.upvalues = r.byte(); p.variadic = r.byte();
        require(p.stack && p.params <= p.stack && p.variadic <= 1, "Invalid prototype header");
        unsigned flags = version >= 4 ? r.byte() : 0;
        if (version >= 4) {
            unsigned size = r.count(); size_t start = r.pos; r.skip(size);
            if (size && types == 1) require(size == p.params + 2, "Invalid type information");
            if (size && types >= 2) {
                Reader t{bytes.data() + start, size};
                unsigned function = t.count(), upvalues = t.count(), locals = t.count();
                t.skip(function); t.skip(upvalues);
                for (unsigned j = 0; j < locals; j++) { t.skip(2); t.var(); t.var(); }
                require(t.pos == size, "Invalid type information");
            }
        }
        unsigned code = r.count(); require(code > 0 && code <= (r.size - r.pos) / 4, "Invalid instruction count");
        size_t start = r.pos; p.code.reserve(code);
        for (unsigned j = 0; j < code; j++) p.code.push_back(r.word());
        for (size_t pc = 0; pc < code;) {
            unsigned op = (p.code[pc] & 255) * multiplier & 255;
            require(op < LOP__COUNT, "Unknown opcode (not standard Luau or Roblox encoded bytecode)");
            p.code[pc] = (p.code[pc] & ~255u) | op; bytes[start + pc * 4] = op;
            pc += Luau::getOpLength(LuauOpcode(op)); require(pc <= code, "Missing instruction AUX word");
        }
        unsigned constants = r.count(); p.constants.reserve(constants); p.closures.resize(constants);
        for (unsigned j = 0; j < constants; j++) {
            unsigned kind = r.byte(); p.constants.push_back(kind);
            auto key = [&]() { unsigned k = r.var(); require(k < j && p.constants[k] == LBC_CONSTANT_STRING, "Unsupported table template key"); };
            switch (kind) {
            case LBC_CONSTANT_NIL: break;
            case LBC_CONSTANT_BOOLEAN: require(r.byte() <= 1, "Invalid boolean constant"); break;
            case LBC_CONSTANT_NUMBER: r.skip(8); break;
            case LBC_CONSTANT_STRING: r.string(strings, false); break;
            case LBC_CONSTANT_VECTOR: r.skip(16); break;
            case LBC_CONSTANT_VECTORD: r.skip(32); break;
            case LBC_CONSTANT_IMPORT: {
                uint32_t id = r.word(); require(id >> 30, "Empty import path");
                for (unsigned n = 0; n < (id >> 30); n++) { unsigned k = (id >> (20 - 10 * n)) & 1023; require(k < j && p.constants[k] == LBC_CONSTANT_STRING, "Invalid import path"); }
                break;
            }
            case LBC_CONSTANT_TABLE: case LBC_CONSTANT_TABLE_WITH_CONSTANTS: {
                unsigned keys = r.count();
                for (unsigned n = 0; n < keys; n++) { key(); if (kind == LBC_CONSTANT_TABLE_WITH_CONSTANTS) { auto k = int32_t(r.word()); require(k == -1 || (k >= 0 && unsigned(k) < j), "Invalid template value"); } }
                break;
            }
            case LBC_CONSTANT_CLOSURE: p.closures[j] = r.var(); require(p.closures[j] < i, "Invalid closure prototype"); break;
            case LBC_CONSTANT_INTEGER: require(r.byte() <= 1, "Invalid integer sign"); r.var64(); break;
            default: throw std::runtime_error("Unsupported constant kind");
            }
        }
        unsigned children = r.count();
        for (unsigned j = 0; j < children; j++) { unsigned child = r.var(); require(child < i, "Invalid child prototype"); p.children.push_back(child); }
        r.var(); r.string(strings);
        if (r.byte()) { unsigned gap = r.byte(); require(gap < 32, "Invalid line interval"); r.skip(code); r.skip((((code - 1) >> gap) + 1) * size_t(4)); }
        if (r.byte()) {
            unsigned locals = r.count();
            for (unsigned j = 0; j < locals; j++) { r.string(strings); unsigned from = r.var(), to = r.var(); require(from <= to && to <= code, "Invalid local debug range"); require(r.byte() < p.stack, "Invalid local debug register"); }
            unsigned upvalues = r.count(); require(upvalues == p.upvalues, "Invalid debug upvalue count");
            for (unsigned j = 0; j < upvalues; j++) r.string(strings);
        }
        if (version >= 11) { unsigned feedback = r.count(); for (unsigned j = 0; j < feedback; j++) { require(r.byte() == 0, "Invalid feedback kind"); require(r.var() < code, "Invalid feedback location"); } }
        if (version >= 12) { if (flags & LPF_INLINABLE) r.var64(); require(r.pos <= end, "Invalid prototype size"); r.pos = end; }
        instructions(p, protos); protos.push_back(std::move(p));
    }
    unsigned main = r.var(); require(main < count && protos[main].upvalues == 0, "Invalid main prototype");
    // Roblox dumps can append a 24-byte bytecode hash. It is metadata, not instructions.
    require(r.pos == r.size || (multiplier == 203 && r.size - r.pos == 24), "Unexpected bytes after main prototype");
    return r.pos;
}
}
extern "C" int emu_prepare_bytecode(unsigned char* data, size_t* size, char* error, size_t capacity) {
    std::string messages;
    for (unsigned multiplier : {1u, 203u}) {
        try {
            std::vector<unsigned char> bytes(data, data + *size);
            size_t end = prepare(bytes, multiplier); memcpy(data, bytes.data(), end); *size = end;
            return multiplier == 1 ? 1 : 2;
        } catch (const std::exception& e) {
            if (!messages.empty()) messages += "; Roblox: "; else messages = "Luau: ";
            messages += e.what();
        }
    }
    if (capacity) { size_t n = std::min(messages.size(), capacity - 1); memcpy(error, messages.data(), n); error[n] = 0; }
    return 0;
}
