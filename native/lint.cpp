#include "Luau/Parser.h"
#include "Luau/LinterConfig.h"

#include <algorithm>
#include <exception>
#include <string>
#include <vector>

namespace Luau
{
std::vector<LintWarning> lintAst(
    AstStat* root, const AstNameTable& names, const std::vector<std::string>& globals,
    const std::vector<HotComment>& hotcomments, const LintOptions& options);
}

using DiagnosticCallback = void (*)(void*, unsigned, unsigned, unsigned, unsigned,
    bool, const char*, size_t, const char*, size_t);

extern "C" int emu_lint(const char* source, size_t length, const char* const* globals,
    size_t globalCount, void* context, DiagnosticCallback callback)
{
    if (!source || length > 256 * 1024 || globalCount > 512 || !callback)
        return 1;

    try
    {
        Luau::Allocator allocator;
        Luau::AstNameTable names(allocator);
        Luau::ParseResult parsed = Luau::Parser::parse(source, length, names, allocator);
        size_t emitted = 0;
        auto emit = [&](const Luau::Location& location, bool error, const std::string& message, const char* code)
        {
            if (emitted++ >= 100)
                return;
            callback(context, location.begin.line, location.begin.column, location.end.line, location.end.column,
                error, message.data(), std::min(message.size(), size_t(8192)), code, std::char_traits<char>::length(code));
        };

        for (const Luau::ParseError& error : parsed.errors)
            emit(error.getLocation(), true, error.getMessage(), "SyntaxError");

        if (!parsed.errors.empty() || !parsed.root)
            return 0;

        std::vector<std::string> knownGlobals;
        knownGlobals.reserve(globalCount);
        for (size_t i = 0; i < globalCount; ++i)
            knownGlobals.emplace_back(globals[i]);

        Luau::LintOptions options;
        options.setDefaults();
        options.warningMask &= ~Luau::LintWarning::parseMask(parsed.hotcomments);
        options.disableWarning(Luau::LintWarning::Code_UnknownType);
        options.disableWarning(Luau::LintWarning::Code_DeprecatedApi);
        options.disableWarning(Luau::LintWarning::Code_TableOperations);
        for (const Luau::LintWarning& warning : Luau::lintAst(parsed.root, names, knownGlobals, parsed.hotcomments, options))
            emit(warning.location, false, warning.text, Luau::LintWarning::getName(warning.code));
        return 0;
    }
    catch (const std::exception&)
    {
        return 2;
    }
    catch (...)
    {
        return 2;
    }
}
