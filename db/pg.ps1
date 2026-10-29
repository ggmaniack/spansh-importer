# Usage: .\db\pg.ps1 start|stop|status|restart
& F:\pgsql\bin\pg_ctl.exe -D F:\pgdata -l F:\pgdata\pg_ctl.log -w @args
