#!/bin/sh
set -eu
umask 077

bundle_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$bundle_dir"
os_name=$(uname -s)

case "$os_name" in
    Linux)
        live_bin=/opt/inori/inori-server
        bin_dir=/opt/inori
        data_dir=/var/lib/inori
        backup_dir=/var/backups/inori
        unit_file=/etc/systemd/system/inori.service
        service_user=inori
        service_group=inori
        command -v systemctl >/dev/null 2>&1 || { echo "systemd が必要です。" >&2; exit 1; }
        stop_service() { systemctl stop inori.service; }
        force_stop() { systemctl stop inori.service >/dev/null 2>&1 || true; }
        ;;
    Darwin)
        live_bin=/usr/local/lib/inori/inori-server
        bin_dir=/usr/local/lib/inori
        data_dir="/Library/Application Support/Inori/server"
        backup_dir="/Library/Application Support/Inori/backups"
        unit_file=/Library/LaunchDaemons/dev.inori.server.plist
        service_user=_inori
        service_group=_inori
        command -v launchctl >/dev/null 2>&1 || { echo "launchctl が必要です。" >&2; exit 1; }
        stop_service() {
            if launchctl print system/dev.inori.server >/dev/null 2>&1; then
                launchctl bootout system/dev.inori.server
            fi
        }
        force_stop() { launchctl bootout system/dev.inori.server >/dev/null 2>&1 || true; }
        ;;
    *)
        echo "Linux または macOS で実行してください。" >&2
        exit 1
        ;;
esac

if [ "$(id -u)" -ne 0 ]; then
    echo "管理者権限で実行してください。開発コマンドからは sudo で実行されます。" >&2
    exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c SHA256SUMS
elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c SHA256SUMS
else
    echo "SHA256検査ツールがありません。" >&2
    exit 1
fi

if [ ! -x "$bundle_dir/inori-server" ] || [ ! -x "$live_bin" ] || \
   [ ! -f "$data_dir/inori.sqlite" ] || [ ! -f "$data_dir/server.toml" ] || \
   [ ! -f "$unit_file" ]; then
    echo "常駐登録済みのサーバーが見つかりません。初回導入は setup.sh --autostart を使ってください。" >&2
    exit 1
fi

lock_dir=$bin_dir/.update-in-progress
next_bin=$bin_dir/inori-server.new
previous_bin=$bin_dir/inori-server.previous
rollback_db=$data_dir/inori.sqlite.rollback
if [ -e "$lock_dir" ] || [ -e "$next_bin" ] || [ -e "$previous_bin" ] || [ -e "$rollback_db" ]; then
    echo "前回更新の一時ファイルがあります。状態を確認してから再実行してください。" >&2
    exit 1
fi

mkdir "$lock_dir"
needs_start=0
committed=0
backup_file=$backup_dir/update-$(date -u +%Y%m%dT%H%M%SZ)-$$.sqlite

restore_database() {
    restore_dir=$(mktemp -d "$data_dir/.update-restore.XXXXXX") || return 1
    if ! chmod 0700 "$restore_dir"; then
        rmdir "$restore_dir" 2>/dev/null || true
        return 1
    fi
    if ! "$live_bin" --data-dir "$restore_dir" --config "$data_dir/server.toml" restore "$backup_file"; then
        rm -f "$restore_dir/inori.sqlite" "$restore_dir/inori.sqlite-wal" "$restore_dir/inori.sqlite-shm" "$restore_dir/server.lock"
        rmdir "$restore_dir" 2>/dev/null || true
        return 1
    fi
    if ! install -o "$service_user" -g "$service_group" -m 0600 "$restore_dir/inori.sqlite" "$rollback_db"; then
        rm -f "$restore_dir/inori.sqlite" "$restore_dir/inori.sqlite-wal" "$restore_dir/inori.sqlite-shm" "$restore_dir/server.lock"
        rmdir "$restore_dir" 2>/dev/null || true
        return 1
    fi
    rm -f "$data_dir/inori.sqlite-wal" "$data_dir/inori.sqlite-shm"
    if ! mv -f "$rollback_db" "$data_dir/inori.sqlite"; then
        rm -f "$restore_dir/inori.sqlite" "$restore_dir/inori.sqlite-wal" "$restore_dir/inori.sqlite-shm" "$restore_dir/server.lock"
        rmdir "$restore_dir" 2>/dev/null || true
        return 1
    fi
    rm -f "$restore_dir/inori.sqlite" "$restore_dir/inori.sqlite-wal" "$restore_dir/inori.sqlite-shm" "$restore_dir/server.lock"
    rmdir "$restore_dir" 2>/dev/null || true
}

finish() {
    result=$1
    trap - EXIT HUP INT TERM
    if [ "$committed" -eq 0 ] && [ "$needs_start" -eq 1 ]; then
        force_stop
        if [ -f "$previous_bin" ]; then
            if mv -f "$previous_bin" "$live_bin"; then
                if restore_database; then
                    if "$live_bin" --data-dir "$data_dir" service start; then
                        echo "更新に失敗したため、旧バイナリと更新前DBへ戻しました。"
                    else
                        echo "旧バージョンを戻しましたが、サービスの起動に失敗しました。" >&2
                        echo "DBバックアップ: $backup_file" >&2
                    fi
                else
                    echo "旧バイナリは戻しましたが、DBの自動復元に失敗しました。" >&2
                    echo "サービスは停止したままです。DBバックアップ: $backup_file" >&2
                fi
            else
                echo "旧バイナリを戻せませんでした。サービスは停止したままです。" >&2
                echo "DBバックアップ: $backup_file" >&2
            fi
        else
            if ! "$live_bin" --data-dir "$data_dir" service start; then
                echo "更新前のサービスを再起動できませんでした。DBバックアップ: $backup_file" >&2
            fi
        fi
    fi
    rm -f "$next_bin"
    if [ "$committed" -eq 1 ]; then
        rm -f "$previous_bin"
    fi
    rmdir "$lock_dir" 2>/dev/null || true
    if [ "$committed" -eq 1 ]; then
        echo "更新が完了しました。DBバックアップ: $backup_file"
    elif [ "$result" -ne 0 ] && [ -f "$backup_file" ]; then
        echo "更新前のDBバックアップを保持しました: $backup_file" >&2
    fi
    exit "$result"
}

trap 'finish $?' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

install -m 0755 "$bundle_dir/inori-server" "$next_bin"
needs_start=1
stop_service
if [ "$os_name" = Darwin ]; then
    install -d -o root -g wheel -m 0700 "$backup_dir"
else
    install -d -o root -g root -m 0700 "$backup_dir"
fi
"$live_bin" --data-dir "$data_dir" backup "$backup_file"
chmod 0600 "$backup_file"

mv "$live_bin" "$previous_bin"
mv "$next_bin" "$live_bin"
"$live_bin" --data-dir "$data_dir" service start
committed=1
needs_start=0
