# Define the path to your WSL script
$scriptPath = "/mnt/c/Users/akosv/RustroverProjects/mineshieldv2-proxy/build-wsl.sh"

# Check if WSL is installed and accessible
if (!(Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
    Write-Host "WSL is not installed or not accessible. Please install WSL2 and try again." -ForegroundColor Red
    exit 1
}

# Execute the build script in Ubuntu-22.04 on WSL2, ensuring the environment is sourced
Write-Host "Running the build script in Ubuntu-22.04 on WSL2..."
wsl -d Ubuntu-22.04 bash -c "source ~/.cargo/env && bash $scriptPath"

# Capture the exit code of the WSL command
$exitCode = $LASTEXITCODE

# Check if the script was successful
if ($exitCode -eq 0) {
    Write-Host "Script executed successfully. Check the output directory for the built binary." -ForegroundColor Green
} else {
    Write-Host "Script execution failed with exit code $exitCode." -ForegroundColor Red
}
