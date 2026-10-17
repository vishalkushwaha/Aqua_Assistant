; Run by the NSIS installer (bundle.windows.nsis.installerHooks). After the files are copied it offers to download the
; models (about 6.5 GB, too big to carry inside an installer), and leaves a Start menu entry to do it again later.
; The same script works if the download is interrupted: finished files are kept and unfinished ones resume.

!macro NSIS_HOOK_POSTINSTALL
  CreateShortcut "$SMPROGRAMS\Download Aqua models.lnk" "$WINDIR\System32\WindowsPowerShell\v1.0\powershell.exe" '-NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\setup\install_models.ps1"'
  CreateShortcut "$SMPROGRAMS\Setup Aqua CUDA & GPU.lnk" "$WINDIR\System32\WindowsPowerShell\v1.0\powershell.exe" '-NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\setup\enable_gpu.ps1"'
  IfSilent aqua_models_done
  MessageBox MB_YESNO|MB_ICONQUESTION "Aqua needs its speech and language models and will automatically detect and configure NVIDIA GPU & CUDA acceleration.$\r$\n$\r$\nDownload and set up now? A window shows the progress.$\r$\n$\r$\nChoose No to do it later from the Start menu ($\"Download Aqua models$\")." IDNO aqua_models_done
  ExecWait '"$WINDIR\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\setup\install_models.ps1"'
  aqua_models_done:
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  Delete "$SMPROGRAMS\Download Aqua models.lnk"
  Delete "$SMPROGRAMS\Setup Aqua CUDA & GPU.lnk"
!macroend
