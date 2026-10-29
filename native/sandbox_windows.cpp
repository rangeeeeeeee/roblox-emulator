#if defined(_WIN32)
#define WIN32_LEAN_AND_MEAN
#ifdef _WIN32_WINNT
#undef _WIN32_WINNT
#endif
#define _WIN32_WINNT 0x0A00
#include <windows.h>
#include <userenv.h>
#include <sddl.h>
#include <algorithm>
#include <cstring>
#include <cwchar>
#include <memory>
#include <new>
#include <string>
#include <vector>

// The only inherited handles are the two pipe ends. A failed policy, token or
// job check must never fall back to an unrestricted process.
struct EmuSandboxHandles
{
    void* sandbox;
    void* input;
    void* output;
};

namespace {
constexpr unsigned long long MAX_WORKER_MEMORY = 8ull * 1024 * 1024 * 1024;
void close_handle(HANDLE& handle)
{
    if (handle && handle != INVALID_HANDLE_VALUE) CloseHandle(handle);
    handle = nullptr;
}

struct Sandbox
{
    HANDLE job = nullptr;
    HANDLE process = nullptr;
    HANDLE thread = nullptr;
    HANDLE input_read = nullptr;
    HANDLE input_write = nullptr;
    HANDLE output_read = nullptr;
    HANDLE output_write = nullptr;
    PSID sid = nullptr;
    std::wstring profile;
    bool created_profile = false;

    ~Sandbox()
    {
        // KILL_ON_JOB_CLOSE prevents a worker from surviving its controller.
        if (process && job) TerminateJobObject(job, 1);
        // The process might have failed before it was assigned to the job.
        if (process) TerminateProcess(process, 1);
        close_handle(thread);
        close_handle(job);
        if (process) WaitForSingleObject(process, 5000);
        close_handle(process);
        close_handle(input_read);
        close_handle(input_write);
        close_handle(output_read);
        close_handle(output_write);
        if (created_profile) DeleteAppContainerProfile(profile.c_str());
        if (sid) FreeSid(sid);
    }
};

struct AttributeList
{
    std::vector<unsigned char> storage;
    LPPROC_THREAD_ATTRIBUTE_LIST list = nullptr;
    ~AttributeList() { if (list) DeleteProcThreadAttributeList(list); }
    bool initialize(DWORD count)
    {
        SIZE_T size = 0;
        InitializeProcThreadAttributeList(nullptr, count, 0, &size);
        if (GetLastError() != ERROR_INSUFFICIENT_BUFFER || !size) return false;
        storage.resize(size);
        auto candidate = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage.data());
        if (!InitializeProcThreadAttributeList(candidate, count, 0, &size)) return false;
        list = candidate;
        return true;
    }
    bool set(DWORD_PTR attribute, void* value, SIZE_T size)
    {
        return UpdateProcThreadAttribute(list, 0, attribute, value, size, nullptr, nullptr) != FALSE;
    }
};

DWORD get_token_data(HANDLE token, TOKEN_INFORMATION_CLASS kind, std::vector<unsigned char>& bytes)
{
    DWORD size = 0;
    if (GetTokenInformation(token, kind, nullptr, 0, &size) || GetLastError() != ERROR_INSUFFICIENT_BUFFER || !size || size > 65536)
        return ERROR_INVALID_DATA;
    bytes.resize(size);
    if (!GetTokenInformation(token, kind, bytes.data(), size, &size)) return GetLastError();
    return ERROR_SUCCESS;
}

void set_stage(char* stage, size_t size, const char* value);

DWORD verify_token(HANDLE process, PSID expected, char* stage, size_t stage_size)
{
    HANDLE token = nullptr;
    set_stage(stage, stage_size, "OpenProcessToken");
    if (!OpenProcessToken(process, TOKEN_QUERY | TOKEN_DUPLICATE, &token)) return GetLastError();
    DWORD error = ERROR_SUCCESS;
    DWORD returned = 0;
    DWORD is_appcontainer = 0;
    set_stage(stage, stage_size, "TokenIsAppContainer");
    if (!GetTokenInformation(token, TokenIsAppContainer, &is_appcontainer, sizeof(is_appcontainer), &returned)) error = GetLastError();
    else if (!is_appcontainer) error = ERROR_ACCESS_DENIED;
    std::vector<unsigned char> bytes;
    if (!error) {
        set_stage(stage, stage_size, "TokenAppContainerSid");
        error = get_token_data(token, TokenAppContainerSid, bytes);
        if (!error) {
            auto info = reinterpret_cast<TOKEN_APPCONTAINER_INFORMATION*>(bytes.data());
            if (!info->TokenAppContainer || !IsValidSid(info->TokenAppContainer)
                || (expected && !EqualSid(info->TokenAppContainer, expected))) error = ERROR_ACCESS_DENIED;
        }
    }
    if (!error) {
        // Test the ALL APPLICATION PACKAGES opt-out via AccessCheck, as Chromium
        // does; querying TokenIsLessPrivilegedAppContainer may fail with 87.
        // https://chromium.googlesource.com/chromium/src/+/2a4d05dea58d1a561f880e523abff7188095d69e/sandbox/win/src/app_container_test.cc
        set_stage(stage, stage_size, "verify LPAC access policy");
        HANDLE impersonation = nullptr;
        PSECURITY_DESCRIPTOR descriptor = nullptr;
        if (!DuplicateToken(token, SecurityImpersonation, &impersonation)) error = GetLastError();
        else if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(
            L"O:SYG:SYD:(A;;0x3;;;WD)(A;;0x1;;;AC)(A;;0x2;;;S-1-15-2-2)",
            SDDL_REVISION_1, &descriptor, nullptr)) error = GetLastError();
        else {
            GENERIC_MAPPING mapping{};
            PRIVILEGE_SET privileges{};
            DWORD privilege_size = sizeof(privileges);
            DWORD granted = 0;
            BOOL allowed = FALSE;
            if (!AccessCheck(descriptor, impersonation, MAXIMUM_ALLOWED, &mapping, &privileges, &privilege_size, &granted, &allowed))
                error = GetLastError();
            else if (!allowed || granted != 2) error = ERROR_ACCESS_DENIED;
        }
        if (descriptor) LocalFree(descriptor);
        close_handle(impersonation);
    }
    if (!error) {
        set_stage(stage, stage_size, "TokenCapabilities");
        error = get_token_data(token, TokenCapabilities, bytes);
        if (!error && reinterpret_cast<TOKEN_GROUPS*>(bytes.data())->GroupCount != 0) error = ERROR_ACCESS_DENIED;
    }
    if (!error) {
        set_stage(stage, stage_size, "TokenIntegrityLevel");
        error = get_token_data(token, TokenIntegrityLevel, bytes);
        if (!error) {
            PSID integrity = reinterpret_cast<TOKEN_MANDATORY_LABEL*>(bytes.data())->Label.Sid;
            if (!integrity || !IsValidSid(integrity) || !*GetSidSubAuthorityCount(integrity)
                || *GetSidSubAuthority(integrity, *GetSidSubAuthorityCount(integrity) - 1) > SECURITY_MANDATORY_LOW_RID)
                error = ERROR_ACCESS_DENIED;
        }
    }
    CloseHandle(token);
    return error;
}

DWORD verify_mitigations(HANDLE process)
{
    PROCESS_MITIGATION_CHILD_PROCESS_POLICY children{};
    if (!GetProcessMitigationPolicy(process, ProcessChildProcessPolicy, &children, sizeof(children))) return GetLastError();
    if (!children.NoChildProcessCreation) return ERROR_ACCESS_DENIED;
    PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY calls{};
    if (!GetProcessMitigationPolicy(process, ProcessSystemCallDisablePolicy, &calls, sizeof(calls))) return GetLastError();
    if (!calls.DisallowWin32kSystemCalls) return ERROR_ACCESS_DENIED;
    return ERROR_SUCCESS;
}

void set_stage(char* stage, size_t size, const char* value)
{
    if (!stage || !size) return;
    std::strncpy(stage, value, size - 1);
    stage[size - 1] = '\0';
}
}

extern "C" DWORD emu_sandbox_spawn(const wchar_t* exe, const wchar_t* profile, EmuSandboxHandles* handles, char* stage, size_t stage_size)
{
    if (!exe || !profile || !handles) return ERROR_INVALID_PARAMETER;
    *handles = {};
    try {
        auto sandbox = std::make_unique<Sandbox>();
        sandbox->profile = profile;
        set_stage(stage, stage_size, "CreateAppContainerProfile");
        HRESULT hr = CreateAppContainerProfile(profile, L"Roblox emulator worker", L"Ephemeral isolated execution", nullptr, 0, &sandbox->sid);
        if (FAILED(hr)) return HRESULT_CODE(hr);
        sandbox->created_profile = true;
        if (!sandbox->sid || !IsValidSid(sandbox->sid)) return ERROR_INVALID_SID;

        set_stage(stage, stage_size, "CreatePipe");
        SECURITY_ATTRIBUTES sa{sizeof(sa), nullptr, TRUE};
        if (!CreatePipe(&sandbox->input_read, &sandbox->input_write, &sa, 0)
            || !CreatePipe(&sandbox->output_read, &sandbox->output_write, &sa, 0)) return GetLastError();
        if (!SetHandleInformation(sandbox->input_write, HANDLE_FLAG_INHERIT, 0)
            || !SetHandleInformation(sandbox->output_read, HANDLE_FLAG_INHERIT, 0)) return GetLastError();

        set_stage(stage, stage_size, "CreateJobObject");
        sandbox->job = CreateJobObjectW(nullptr, nullptr);
        if (!sandbox->job) return GetLastError();
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits{};
        if (sizeof(SIZE_T) < sizeof(unsigned long long)) return ERROR_NOT_SUPPORTED;
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_JOB_MEMORY;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        limits.ProcessMemoryLimit = static_cast<SIZE_T>(MAX_WORKER_MEMORY);
        limits.JobMemoryLimit = static_cast<SIZE_T>(MAX_WORKER_MEMORY);
        if (!SetInformationJobObject(sandbox->job, JobObjectExtendedLimitInformation, &limits, sizeof(limits))) return GetLastError();
        JOBOBJECT_BASIC_UI_RESTRICTIONS ui{};
        ui.UIRestrictionsClass = JOB_OBJECT_UILIMIT_ALL;
        if (!SetInformationJobObject(sandbox->job, JobObjectBasicUIRestrictions, &ui, sizeof(ui))) return GetLastError();

        set_stage(stage, stage_size, "InitializeProcThreadAttributeList");
        AttributeList attributes;
        if (!attributes.initialize(5)) return GetLastError();
        SECURITY_CAPABILITIES capabilities{};
        capabilities.AppContainerSid = sandbox->sid;
        DWORD lpac = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT;
        DWORD no_children = PROCESS_CREATION_CHILD_PROCESS_RESTRICTED;
        DWORD64 mitigation = PROCESS_CREATION_MITIGATION_POLICY_WIN32K_SYSTEM_CALL_DISABLE_ALWAYS_ON;
        HANDLE inherited[] = {sandbox->input_read, sandbox->output_write};
        set_stage(stage, stage_size, "UpdateProcThreadAttribute");
        if (!attributes.set(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &capabilities, sizeof(capabilities))
            || !attributes.set(PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY, &lpac, sizeof(lpac))
            || !attributes.set(PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, &no_children, sizeof(no_children))
            || !attributes.set(PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, &mitigation, sizeof(mitigation))
            || !attributes.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, inherited, sizeof(inherited))) return GetLastError();

        wchar_t system_dir[MAX_PATH + 1];
        wchar_t windows_dir[MAX_PATH + 1];
        UINT system_len = GetSystemDirectoryW(system_dir, MAX_PATH + 1);
        UINT windows_len = GetWindowsDirectoryW(windows_dir, MAX_PATH + 1);
        if (!system_len || system_len > MAX_PATH || !windows_len || windows_len > MAX_PATH) return ERROR_INSUFFICIENT_BUFFER;
        std::wstring command = L"\"";
        if (std::wcschr(exe, L'"')) return ERROR_INVALID_NAME;
        command += exe;
        command += L"\" --isolated-worker";
        // Windows needs LOCALAPPDATA to resolve the per-user AppContainer profile.
        // Do not inherit the parent's environment, which may contain credentials.
        set_stage(stage, stage_size, "read LOCALAPPDATA");
        DWORD local_size = GetEnvironmentVariableW(L"LOCALAPPDATA", nullptr, 0);
        if (!local_size || local_size > 32768) return ERROR_ENVVAR_NOT_FOUND;
        std::vector<wchar_t> local(local_size);
        if (!GetEnvironmentVariableW(L"LOCALAPPDATA", local.data(), local_size)) return GetLastError();
        std::wstring environment = L"LOCALAPPDATA=";
        environment += local.data();
        environment.push_back(L'\0');
        environment += L"SystemRoot=";
        environment += windows_dir;
        environment.push_back(L'\0');
        environment.push_back(L'\0');

        STARTUPINFOEXW startup{};
        startup.StartupInfo.cb = sizeof(startup);
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = sandbox->input_read;
        startup.StartupInfo.hStdOutput = sandbox->output_write;
        startup.StartupInfo.hStdError = nullptr;
        startup.lpAttributeList = attributes.list;
        PROCESS_INFORMATION process{};
        set_stage(stage, stage_size, "CreateProcessW");
        if (!CreateProcessW(exe, command.data(), nullptr, nullptr, TRUE,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | DETACHED_PROCESS | CREATE_UNICODE_ENVIRONMENT,
            environment.data(), system_dir, &startup.StartupInfo, &process)) return GetLastError();
        sandbox->process = process.hProcess;
        sandbox->thread = process.hThread;
        close_handle(sandbox->input_read);
        close_handle(sandbox->output_write);
        set_stage(stage, stage_size, "verify sandbox token");
        if (DWORD error = verify_token(sandbox->process, sandbox->sid, stage, stage_size)) return error;
        set_stage(stage, stage_size, "verify process mitigations");
        if (DWORD error = verify_mitigations(sandbox->process)) return error;
        set_stage(stage, stage_size, "AssignProcessToJobObject");
        if (!AssignProcessToJobObject(sandbox->job, sandbox->process)) return GetLastError();
        BOOL is_in_job = FALSE;
        if (!IsProcessInJob(sandbox->process, sandbox->job, &is_in_job)) return GetLastError();
        if (!is_in_job) return ERROR_ACCESS_DENIED;
        set_stage(stage, stage_size, "ResumeThread");
        if (ResumeThread(sandbox->thread) == static_cast<DWORD>(-1)) return GetLastError();
        close_handle(sandbox->thread);

        handles->input = sandbox->input_write;
        handles->output = sandbox->output_read;
        sandbox->input_write = nullptr;
        sandbox->output_read = nullptr;
        handles->sandbox = sandbox.release();
        set_stage(stage, stage_size, "");
        return ERROR_SUCCESS;
    } catch (const std::bad_alloc&) { return ERROR_NOT_ENOUGH_MEMORY; }
      catch (...) { return ERROR_UNHANDLED_EXCEPTION; }
}

extern "C" void emu_sandbox_close(void* sandbox)
{
    delete static_cast<Sandbox*>(sandbox);
}

extern "C" DWORD emu_sandbox_verify_current(char* stage, size_t stage_size)
{
    try {
        set_stage(stage, stage_size, "verify sandbox token");
        if (DWORD error = verify_token(GetCurrentProcess(), nullptr, stage, stage_size)) return error;
        set_stage(stage, stage_size, "verify sandbox job");
        BOOL is_in_job = FALSE;
        if (!IsProcessInJob(GetCurrentProcess(), nullptr, &is_in_job)) return GetLastError();
        if (!is_in_job) return ERROR_ACCESS_DENIED;
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits{};
        if (!QueryInformationJobObject(nullptr, JobObjectExtendedLimitInformation, &limits, sizeof(limits), nullptr)) return GetLastError();
        constexpr DWORD required = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_JOB_MEMORY;
        if ((limits.BasicLimitInformation.LimitFlags & required) != required || limits.BasicLimitInformation.ActiveProcessLimit != 1
            || limits.ProcessMemoryLimit != MAX_WORKER_MEMORY || limits.JobMemoryLimit != MAX_WORKER_MEMORY)
            return ERROR_ACCESS_DENIED;
        JOBOBJECT_BASIC_UI_RESTRICTIONS ui{};
        if (!QueryInformationJobObject(nullptr, JobObjectBasicUIRestrictions, &ui, sizeof(ui), nullptr)) return GetLastError();
        if ((ui.UIRestrictionsClass & JOB_OBJECT_UILIMIT_ALL) != JOB_OBJECT_UILIMIT_ALL) return ERROR_ACCESS_DENIED;
        set_stage(stage, stage_size, "verify process mitigations");
        if (DWORD error = verify_mitigations(GetCurrentProcess())) return error;
        set_stage(stage, stage_size, "");
        return ERROR_SUCCESS;
    } catch (const std::bad_alloc&) { return ERROR_NOT_ENOUGH_MEMORY; }
      catch (...) { return ERROR_UNHANDLED_EXCEPTION; }
}
#endif
