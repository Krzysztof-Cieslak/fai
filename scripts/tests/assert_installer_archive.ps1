param(
    [Parameter(Mandatory = $true)][string]$InstallerScript,
    [Parameter(Mandatory = $true)][string]$ArchivePath
)

$ErrorActionPreference = 'Stop'
$Tokens = $null
$ParseErrors = $null
$Tree = [System.Management.Automation.Language.Parser]::ParseFile($InstallerScript, [ref]$Tokens, [ref]$ParseErrors)
if ($ParseErrors.Count -ne 0) { throw ($ParseErrors -join "`n") }
$Definition = $Tree.Find({
    param($Node)
    $Node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $Node.Name -eq 'Assert-FaiInstallerArchive'
}, $true)
if ($null -eq $Definition) { throw 'Installer archive validator was not found' }
. ([ScriptBlock]::Create($Definition.Extent.Text))
Assert-FaiInstallerArchive -ArchivePath $ArchivePath
