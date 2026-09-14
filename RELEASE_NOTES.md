# Flow Panel v1.7

���� Go �� Linux / Alpine ������壬��ҳ���ã������������ڴ��ж�ȡ������

- ÿ������һ����������֧�ֲ�����������ѭ�����ؼ�������ͣ��
- �� IP ÿ��һ��������д˳����ѯ������ԭ�������˿ڼ� HTTPS SNI��
- ���������Զ����أ������Զ��ָ��ѿ��������ֶ�ֹͣ������ɵ����񱣳�ֹͣ��
- �������ù̶������� `/abc/tasks.json`��Docker ʹ�� `-v /abc:/abc`��
- ҳ��ֻ��ʾ����״̬��ʵʱ���٣������������ļ���

## ����������

`x86_64` ʹ�� `flow-panel-alpine-amd64`��`aarch64` ʹ�� `flow-panel-alpine-arm64`��

```sh
chmod +x flow-panel-alpine-amd64
./flow-panel-alpine-amd64
```

��������� `http://������IP:8080`��������Ʋ���Ҫ���������ļ���`.tar.gz` ��������˵����systemd �� OpenRC �����ļ���`flow-panel-docker.zip` �ṩ˫�ܹ� Docker �������ϡ�

## ��������

Debian / Ubuntu �� systemd ϵͳʹ�� `flow-panel.service`���Ƚ�����װ�� `/usr/local/bin/flow-panel`����ִ�У�

```sh
install -m 644 flow-panel.service /etc/systemd/system/flow-panel.service
systemctl daemon-reload
systemctl enable --now flow-panel
```

Alpine Ĭ��ʹ�� `flow-panel.openrc`����װ������ README�����ù̶�д�� `/abc/tasks.json`��

## ��֤

39 �� Go ����ͨ���������ܹ���ͨ����̬ ELF ��飬������ glibc / musl ��̬�⡣��ǰ��֤����Ϊ Windows��Linux / Alpine ʵ�����м� Docker ������δ��֤������У��ֵ�� `SHA256SUMS.txt`��
