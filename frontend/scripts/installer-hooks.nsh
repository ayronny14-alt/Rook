!macro customInstall
  nsExec::Exec 'taskkill /F /IM "rook.exe" /T'
!macroend

!macro customUnInstall
  nsExec::Exec 'taskkill /F /IM "rook.exe" /T'
  nsExec::Exec 'taskkill /F /IM "Rook.exe" /T'
!macroend
