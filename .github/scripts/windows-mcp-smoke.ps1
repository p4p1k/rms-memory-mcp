param([Parameter(Mandatory)][string]$Exe)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$Exe = (Resolve-Path -LiteralPath $Exe).Path
$state = Join-Path ([Environment]::GetFolderPath('UserProfile')) '.rms-memory'
# --version may have created rms.log; no registry or database may preexist.
if ((Test-Path "$state/registry.toml") -or (Test-Path "$state/dbs") -or
    (Test-Path "$state/read-only-vaults")) { throw 'Runner RMS profile is not clean' }
$lab = Join-Path $env:RUNNER_TEMP 'rms-mcp-smoke'
if (Test-Path -LiteralPath $lab) { throw 'Smoke directory already exists' }
$project = Join-Path $lab 'RmsPatchSmoke'
$vault = Join-Path $lab 'Vaults/RmsPatchSmoke'
$note = Join-Path $vault 'decisions/rms-isolated-test.md'
New-Item -ItemType Directory -Path $project | Out-Null
$env:RMS_MEMORY_NO_UPDATE_CHECK = '1'
$env:RMS_MEMORY_EMBEDDING_MODEL = 'multilingual-e5-small'
Push-Location -LiteralPath $project
$process = $null
try {
    & $Exe config --vault-path (Join-Path $lab 'Vaults') --auto-add false --inject-rules false --auto-import skip
    if ($LASTEXITCODE -ne 0) { throw 'Smoke configuration failed' }
    & $Exe init
    if ($LASTEXITCODE -ne 0) { throw 'Smoke project registration failed' }
    $registry = Get-Content -Raw -LiteralPath "$state/registry.toml"
    if ($registry -notmatch '\[projects\.RmsPatchSmoke\]') { throw 'Test project not registered' }
    if (-not (Test-Path -LiteralPath $vault -PathType Container)) { throw 'Test Vault missing' }

    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $Exe
    $start.ArgumentList.Add('serve')
    $start.WorkingDirectory = $project
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.StandardInputEncoding = [Text.UTF8Encoding]::new($false)
    $start.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $start
    if (-not $process.Start()) { throw 'MCP did not start' }
    $stderr = $process.StandardError.ReadToEndAsync()
    $script:rpcId = 0
    function Send-Rpc([string]$Method, [hashtable]$Params, [switch]$Notification) {
        $message = @{ jsonrpc = '2.0'; method = $Method; params = $Params }
        if (-not $Notification) { $script:rpcId++; $message.id = $script:rpcId }
        $process.StandardInput.WriteLine((ConvertTo-Json -InputObject $message -Depth 20 -Compress))
        $process.StandardInput.Flush()
        if ($Notification) { return }
        $pending = $process.StandardOutput.ReadLineAsync()
        if (-not $pending.Wait(900000)) { throw "MCP timeout: $Method" }
        $line = $pending.GetAwaiter().GetResult()
        if ($null -eq $line) { throw "MCP exited before response: $Method" }
        Write-Host "MCP $Method : $line"
        $reply = ConvertFrom-Json -InputObject $line -AsHashtable
        if ($reply.id -ne $script:rpcId -or $reply.jsonrpc -ne '2.0') { throw 'Unexpected RPC response' }
        if ($reply.ContainsKey('error')) { throw ($reply.error | ConvertTo-Json -Compress) }
        if (-not $reply.ContainsKey('result')) { throw 'Missing MCP result' }
        return $reply.result
    }
    function Call-Tool([string]$Name, [hashtable]$Arguments) {
        $result = Send-Rpc 'tools/call' @{ name = $Name; arguments = $Arguments }
        if ($result.ContainsKey('isError') -and $result.isError) { throw "Tool failed: $Name" }
        return $result
    }
    function Payload($Result) {
        if ($Result.ContainsKey('structuredContent')) { return $Result.structuredContent }
        return ($Result.content[0].text | ConvertFrom-Json -AsHashtable)
    }
    $null = Send-Rpc 'initialize' @{
        protocolVersion = '2024-11-05'; capabilities = @{}
        clientInfo = @{ name = 'windows-patch-smoke'; version = '1.0' }
    }
    Send-Rpc 'notifications/initialized' @{} -Notification
    $arguments = @{
        project = 'RmsPatchSmoke'; path = 'decisions/rms-isolated-test.md'
        mode = 'create'; content = "# RMS Memory Test`nКонтрольный код: RMS-2710."
        dry_run = $true
    }
    $preview = Payload (Call-Tool 'rms_write' $arguments)
    if ($preview.dry_run -ne $true -or $preview.action -ne 'create') { throw 'Wrong dry-run preview' }
    if (Test-Path -LiteralPath $note) { throw 'Dry-run created a file' }
    Write-Host 'PASS: dry-run leaves note absent'
    $arguments.dry_run = $false
    $created = Payload (Call-Tool 'rms_write' $arguments)
    if ($created.action -ne 'create' -or $created.graph_refresh.status -ne 'ok') { throw 'Create/graph refresh failed' }
    if (-not (Test-Path -LiteralPath $note -PathType Leaf)) { throw 'Create did not create note' }
    if (-not (Get-Content -Raw -LiteralPath $note).Contains($arguments.content)) { throw 'Wrong file content' }
    Write-Host 'PASS: create commits the expected note'
    # rms_read reads indexed chunks. Explicit sync avoids watcher debounce races.
    $null = Call-Tool 'rms_sync' @{ project = 'RmsPatchSmoke' }
    $read = Call-Tool 'rms_read' @{ project = 'RmsPatchSmoke'; path = $arguments.path }
    if ($read.content[0].text -notmatch 'RMS Memory Test' -or
        $read.content[0].text -notmatch 'Контрольный код: RMS-2710\.') { throw 'Read returned wrong content' }
    Write-Host 'PASS: rms_read returns the expected content'
    $indexed = Payload (Call-Tool 'rms_reindex' @{ project = 'RmsPatchSmoke'; corpus = 'vault' })
    if ($indexed.vault_reindexed -ne $true) { throw 'Vault was not reindexed' }
    Write-Host 'PASS: explicit Vault reindex'
    $search = Payload (Call-Tool 'rms_search' @{
        project = 'RmsPatchSmoke'; query = 'RMS-2710'; corpus = 'vault'
        include_content = $true; limit = 10
    })
    $hits = @($search.results | Where-Object {
        ($_.path -replace '\\', '/') -eq $arguments.path -and $_.content -match 'RMS-2710'
    })
    if ($hits.Count -lt 1) { throw 'Search did not return the expected note and code' }
    Write-Host 'PASS: rms_search finds decisions/rms-isolated-test.md with RMS-2710'
    $process.StandardInput.Close()
    if (-not $process.WaitForExit(30000)) { throw 'MCP did not stop after EOF' }
    if ($process.ExitCode -ne 0) { throw 'MCP exited unsuccessfully' }
    'Windows stdio MCP smoke: dry-run, create, read, reindex, search PASS.' >> $env:GITHUB_STEP_SUMMARY
} finally {
    if ($null -ne $process) {
        if (-not $process.HasExited) { $process.Kill($true); $process.WaitForExit() }
        if ($null -ne (Get-Variable stderr -ErrorAction SilentlyContinue)) {
            Write-Host ($stderr.GetAwaiter().GetResult())
        }
        $process.Dispose()
    }
    Pop-Location
}
