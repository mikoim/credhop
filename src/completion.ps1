# credhop completion for Windows PowerShell 5.1 and PowerShell 7.
Register-ArgumentCompleter -Native -CommandName credhop, credhop.exe -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    # The words before the cursor, unquoted. They are passed to credhop as
    # arguments and never evaluated, so a $(...) being typed does not run.
    $words = @(foreach ($element in $commandAst.CommandElements) {
        if ($element.Extent.StartOffset -ge $cursorPosition) { break }
        if ($element.Extent.EndOffset -gt $cursorPosition) {
            # The cursor is inside this word: complete it as PowerShell sees it.
            $wordToComplete
            break
        }
        if ($element -is [System.Management.Automation.Language.StringConstantExpressionAst]) {
            $element.Value
        } else {
            $element.Extent.Text
        }
    })

    if ($wordToComplete -eq '') {
        # Windows PowerShell 5.1, PowerShell 7.0-7.2 and the Legacy argument
        # passing mode drop an empty argument; '""' reaches credhop as one.
        $version = $PSVersionTable.PSVersion
        $legacy = $version.Major -lt 7 -or ($version.Major -eq 7 -and $version.Minor -lt 3) -or
            $PSNativeCommandArgumentPassing -eq 'Legacy'
        $words += $(if ($legacy) { '""' } else { '' })
    }

    $previous = $env:COMPLETE
    $env:COMPLETE = 'powershell'
    try {
        $candidates = & credhop '--' @words
    } finally {
        if ($null -eq $previous) {
            Remove-Item Env:\COMPLETE
        } else {
            $env:COMPLETE = $previous
        }
    }

    foreach ($candidate in $candidates) {
        $value, $help = $candidate -split "`t", 2
        if (-not $help) { $help = $value }
        [System.Management.Automation.CompletionResult]::new($value, $value, 'ParameterValue', $help)
    }
}
