#!/usr/bin/env python3
"""Carga CSV de NASA FIRMS (MODIS y VIIRS) a api-nasa.

Hace dos pasos:
  1. Lee uno o varios CSV de FIRMS y genera UN CSV normalizado con las columnas
     de la entidad Detection (y otro con las filas rechazadas, si las hay).
  2. Sube el CSV normalizado a la API en lotes.

Solo usa la biblioteca estándar de Python (3.9+): no hay que instalar nada.

Ejemplos:
  export MARRO_API_URL=https://api.tudominio.com
  export MARRO_API_KEY=tu_clave

  # Normalizar y subir todos los CSV de una carpeta
  python cargar_csv.py datos/

  # Solo generar el CSV normalizado, sin subir
  python cargar_csv.py datos/*.csv --solo-normalizar

  # Subir un CSV ya normalizado
  python cargar_csv.py detecciones_normalizadas.csv --ya-normalizado

Modos de subida (--modo):
  json    (por defecto) POST /detections con un arreglo JSON por lote. Todo o nada por lote:
          si una detección del lote ya existe en la base, la API rechaza el lote con 409.
  import  POST /detections/import con el CSV por lote. Las detecciones que ya existen
          se saltan, así que se puede volver a correr sin problema.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import sys
import time
import urllib.error
import urllib.request
from datetime import date
from pathlib import Path

# Columnas de la entidad Detection, en el orden del CSV normalizado.
COLUMNAS = [
    "latitude", "longitude", "bright_ti4", "scan", "track", "acq_date", "acq_time",
    "satellite", "instrument", "confidence", "version", "bright_ti5", "frp", "daynight",
]
NUMERICAS_OPCIONALES = ["bright_ti4", "scan", "track", "bright_ti5", "frp"]

# Nombres alternativos de columnas (MODIS usa brightness y bright_t31).
ALIAS = {
    "latitude": ["latitude", "lat"],
    "longitude": ["longitude", "lon"],
    "bright_ti4": ["bright_ti4", "brightness"],
    "bright_ti5": ["bright_ti5", "bright_t31"],
}


# ---------------------------------------------------------------- Normalización

def _numero(valor: str, campo: str) -> float | None:
    if valor == "":
        return None
    try:
        n = float(valor)
    except ValueError:
        raise ValueError(f"{campo} no es un número: {valor!r}")
    if n != n or n in (float("inf"), float("-inf")):
        raise ValueError(f"{campo} no es un número válido: {valor!r}")
    return n


def _fmt(n: float | None) -> str:
    """Número a texto sin perder precisión (repr da la forma más corta exacta)."""
    return "" if n is None else repr(n)


def _hora(valor: str) -> str:
    """'412', '0412' o '04:12' -> '0412'."""
    digitos = valor.replace(":", "")
    if not digitos.isdigit() or len(digitos) > 4:
        raise ValueError(f"acq_time debe ser HHMM: {valor!r}")
    digitos = digitos.zfill(4)
    if int(digitos[:2]) > 23 or int(digitos[2:]) > 59:
        raise ValueError(f"acq_time fuera de rango: {valor!r}")
    return digitos


def _fecha(valor: str) -> str:
    """Acepta YYYY-MM-DD (FIRMS) y también YYYY/MM/DD."""
    try:
        return date.fromisoformat(valor.replace("/", "-")).isoformat()
    except ValueError:
        raise ValueError(f"acq_date debe ser YYYY-MM-DD: {valor!r}")


def normalizar_fila(fila: dict[str, str], instrumento_por_defecto: str) -> dict[str, str]:
    """Convierte una fila de FIRMS a las columnas de Detection. Lanza ValueError si es inválida."""

    def tomar(campo: str) -> str:
        for nombre in ALIAS.get(campo, [campo]):
            if nombre in fila and fila[nombre] is not None:
                return fila[nombre].strip()
        return ""

    lat = _numero(tomar("latitude"), "latitude")
    lon = _numero(tomar("longitude"), "longitude")
    if lat is None or not -90 <= lat <= 90:
        raise ValueError("latitude vacía o fuera de -90..90")
    if lon is None or not -180 <= lon <= 180:
        raise ValueError("longitude vacía o fuera de -180..180")

    out = {"latitude": _fmt(lat), "longitude": _fmt(lon)}

    for campo in NUMERICAS_OPCIONALES:
        n = _numero(tomar(campo), campo)
        if n is not None and n < 0:
            raise ValueError(f"{campo} no puede ser negativo: {n}")
        out[campo] = _fmt(n)

    if not tomar("acq_date"):
        raise ValueError("acq_date está vacío")
    if not tomar("acq_time"):
        raise ValueError("acq_time está vacío")
    out["acq_date"] = _fecha(tomar("acq_date"))
    out["acq_time"] = _hora(tomar("acq_time"))

    out["satellite"] = tomar("satellite") or "desconocido"
    out["instrument"] = (tomar("instrument") or instrumento_por_defecto).upper()
    out["confidence"] = tomar("confidence")
    out["version"] = tomar("version")

    dn = tomar("daynight").upper()
    if dn not in ("", "D", "N"):
        raise ValueError(f"daynight debe ser D o N: {dn!r}")
    out["daynight"] = dn

    for campo, maximo in (("satellite", 20), ("instrument", 20), ("confidence", 10), ("version", 20)):
        if len(out[campo]) > maximo:
            raise ValueError(f"{campo} admite como máximo {maximo} caracteres")
    return out


def clave(fila: dict[str, str]) -> tuple:
    """Clave natural de una detección (la misma que usa la base para evitar duplicados)."""
    return (float(fila["latitude"]), float(fila["longitude"]), fila["acq_date"],
            fila["acq_time"], fila["satellite"], fila["instrument"])


def buscar_csv(rutas: list[str]) -> list[Path]:
    archivos: list[Path] = []
    for r in rutas:
        p = Path(r)
        if p.is_dir():
            archivos.extend(sorted(p.rglob("*.csv")))
        elif p.is_file():
            archivos.append(p)
        else:
            sys.exit(f"No existe: {r}")
    return archivos


def normalizar(archivos: list[Path], salida: Path, rechazadas: Path,
               instrumento: str | None, solo_vegetacion: bool) -> int:
    """Genera el CSV normalizado. Devuelve cuántas filas quedaron."""
    vistas: set[tuple] = set()
    total = validas = duplicadas = invalidas = filtradas = 0
    f_rech = w_rech = None

    with salida.open("w", newline="", encoding="utf-8") as f_out:
        w_out = csv.DictWriter(f_out, fieldnames=COLUMNAS)
        w_out.writeheader()

        for archivo in archivos:
            if archivo.resolve() in (salida.resolve(), rechazadas.resolve()):
                continue
            with archivo.open(newline="", encoding="utf-8-sig") as f_in:
                lector = csv.DictReader(f_in)
                columnas = [c.strip().lower() for c in (lector.fieldnames or [])]
                lector.fieldnames = columnas
                if "acq_date" not in columnas or not ({"latitude", "lat"} & set(columnas)):
                    print(f"  {archivo.name}: no parece un CSV de FIRMS, se omite")
                    continue
                # Sin columna instrument: se deduce por las columnas propias de MODIS.
                por_defecto = instrumento or ("MODIS" if "brightness" in columnas else "VIIRS")
                n_archivo = 0
                for n_linea, fila in enumerate(lector, start=2):
                    if not any((v or "").strip() for v in fila.values() if isinstance(v, str)):
                        continue
                    total += 1
                    # 'type' solo viene en los archivos históricos: 0 = fuego de vegetación.
                    if solo_vegetacion and (fila.get("type") or "0").strip() != "0":
                        filtradas += 1
                        continue
                    try:
                        nueva = normalizar_fila(fila, por_defecto)
                    except ValueError as e:
                        invalidas += 1
                        if w_rech is None:
                            f_rech = rechazadas.open("w", newline="", encoding="utf-8")
                            w_rech = csv.writer(f_rech)
                            w_rech.writerow(["archivo", "linea", "error"])
                        w_rech.writerow([archivo.name, n_linea, str(e)])
                        continue
                    k = clave(nueva)
                    if k in vistas:
                        duplicadas += 1
                        continue
                    vistas.add(k)
                    w_out.writerow(nueva)
                    validas += 1
                    n_archivo += 1
                print(f"  {archivo.name}: {n_archivo} filas ({por_defecto if 'instrument' not in columnas else 'instrument del CSV'})")

    if f_rech:
        f_rech.close()

    print(f"\nNormalización: {total} leídas → {validas} válidas, {duplicadas} duplicadas, "
          f"{invalidas} inválidas" + (f", {filtradas} filtradas por type" if solo_vegetacion else ""))
    print(f"CSV normalizado: {salida}")
    if invalidas:
        print(f"Filas rechazadas: {rechazadas}")
    return validas


# ---------------------------------------------------------------- Subida

def _post(url: str, api_key: str, cuerpo: bytes, content_type: str, reintentos: int = 3):
    """POST con reintentos ante fallos de red o errores 5xx. Devuelve (status, json)."""
    for intento in range(1, reintentos + 1):
        req = urllib.request.Request(url, data=cuerpo, method="POST", headers={
            "x-api-key": api_key,
            "Content-Type": content_type,
            "User-Agent": "marro-cargar-csv",
        })
        try:
            with urllib.request.urlopen(req, timeout=300) as resp:
                return resp.status, json.loads(resp.read() or b"{}")
        except urllib.error.HTTPError as e:
            texto = e.read().decode("utf-8", "replace")
            try:
                datos = json.loads(texto)
            except ValueError:
                datos = {"error": texto[:300]}
            if e.code < 500 or intento == reintentos:
                return e.code, datos
        except (urllib.error.URLError, TimeoutError) as e:
            if intento == reintentos:
                sys.exit(f"No se pudo conectar a {url}: {e}")
        time.sleep(2 * intento)
    raise AssertionError("inalcanzable")


def _a_json(fila: dict[str, str]) -> dict:
    """Fila del CSV normalizado -> objeto NewDetection (vacíos como null)."""
    obj: dict = {}
    for campo in COLUMNAS:
        v = fila.get(campo, "")
        if campo in ("latitude", "longitude"):
            obj[campo] = float(v)
        elif campo in NUMERICAS_OPCIONALES:
            obj[campo] = float(v) if v != "" else None
        else:
            obj[campo] = v if v != "" else None
    return obj


def _lotes(ruta: Path, tamano: int):
    with ruta.open(newline="", encoding="utf-8") as f:
        lote: list[dict[str, str]] = []
        for fila in csv.DictReader(f):
            lote.append(fila)
            if len(lote) == tamano:
                yield lote
                lote = []
        if lote:
            yield lote


def subir(ruta: Path, api_url: str, api_key: str, modo: str, tamano: int, desde_lote: int) -> None:
    api_url = api_url.rstrip("/")
    insertadas = repetidas = 0
    primera_fila = 2  # línea del CSV normalizado donde empieza el lote (la 1 es el encabezado)

    for n, lote in enumerate(_lotes(ruta, tamano), start=1):
        if n >= desde_lote:
            if modo == "json":
                cuerpo = json.dumps([_a_json(f) for f in lote]).encode("utf-8")
                status, datos = _post(f"{api_url}/detections", api_key, cuerpo, "application/json")
                if status == 201:
                    insertadas += datos.get("count", len(lote))
            else:
                import io
                buf = io.StringIO()
                w = csv.DictWriter(buf, fieldnames=COLUMNAS)
                w.writeheader()
                w.writerows(lote)
                status, datos = _post(f"{api_url}/detections/import", api_key,
                                      buf.getvalue().encode("utf-8"), "text/csv")
                if status == 200:
                    insertadas += datos.get("inserted", 0)
                    repetidas += datos.get("duplicates", 0)

            if status not in (200, 201):
                print(f"\nLote {n} rechazado (HTTP {status}): {datos.get('error', datos)}")
                if isinstance(datos.get("index"), int):
                    print(f"  Fila con el problema: línea {primera_fila + datos['index']} de {ruta.name}")
                for e in datos.get("errors", [])[:10]:
                    print(f"  línea {primera_fila + e['line'] - 2} de {ruta.name}: {e['error']}")
                if status == 409 and modo == "json":
                    print("  Alguna detección del lote ya existe en la base. Usa --modo import "
                          "para saltar las repetidas.")
                print(f"  Los lotes anteriores ya quedaron guardados. Para continuar: --desde-lote {n}")
                sys.exit(1)
            print(f"  lote {n}: {len(lote)} filas enviadas (HTTP {status})")
        primera_fila += len(lote)

    print(f"\nSubida terminada: {insertadas} detecciones nuevas"
          + (f", {repetidas} ya existían" if modo == "import" else ""))


# ---------------------------------------------------------------- main

def main() -> None:
    ap = argparse.ArgumentParser(description="Normaliza CSV de FIRMS y los sube a api-nasa.")
    ap.add_argument("entradas", nargs="+", help="Archivos CSV o carpetas con CSV de FIRMS")
    ap.add_argument("-o", "--salida", default="detecciones_normalizadas.csv",
                    help="CSV normalizado a generar (por defecto %(default)s)")
    ap.add_argument("--rechazadas", default="detecciones_rechazadas.csv",
                    help="CSV con las filas inválidas y su motivo (por defecto %(default)s)")
    ap.add_argument("--instrumento", help="Instrumento si el CSV no trae la columna (MODIS o VIIRS). "
                                          "Sin esto se deduce de las columnas.")
    ap.add_argument("--solo-vegetacion", action="store_true",
                    help="En archivos históricos, quedarse solo con type = 0 (fuegos de vegetación)")
    ap.add_argument("--solo-normalizar", action="store_true", help="Generar el CSV y no subir nada")
    ap.add_argument("--ya-normalizado", action="store_true",
                    help="La entrada ya es un CSV normalizado: solo subirlo")
    ap.add_argument("--api-url", default=os.environ.get("MARRO_API_URL"),
                    help="URL de la API (o variable MARRO_API_URL)")
    ap.add_argument("--api-key", default=os.environ.get("MARRO_API_KEY"),
                    help="API key (o variable MARRO_API_KEY)")
    ap.add_argument("--modo", choices=["json", "import"], default="json",
                    help="json = POST /detections con arreglos; import = POST /detections/import")
    ap.add_argument("--lote", type=int, default=5000, help="Filas por lote (máx. 10000 en modo json)")
    ap.add_argument("--desde-lote", type=int, default=1, help="Continuar desde este número de lote")
    args = ap.parse_args()

    if not 1 <= args.lote <= 10000:
        sys.exit("--lote debe estar entre 1 y 10000")

    if args.ya_normalizado:
        if len(args.entradas) != 1:
            sys.exit("Con --ya-normalizado indica un solo archivo")
        salida = Path(args.entradas[0])
    else:
        archivos = buscar_csv(args.entradas)
        if not archivos:
            sys.exit("No se encontró ningún CSV")
        salida = Path(args.salida)
        print(f"Normalizando {len(archivos)} archivo(s):")
        validas = normalizar(archivos, salida, Path(args.rechazadas),
                             args.instrumento, args.solo_vegetacion)
        if validas == 0:
            sys.exit("No hay filas válidas para subir")

    if args.solo_normalizar:
        return
    if not args.api_url or not args.api_key:
        sys.exit("Falta la URL o la API key: usa --api-url/--api-key o MARRO_API_URL/MARRO_API_KEY "
                 "(o --solo-normalizar para no subir)")

    print(f"\nSubiendo {salida} a {args.api_url} (modo {args.modo}, lotes de {args.lote}):")
    subir(salida, args.api_url, args.api_key, args.modo, args.lote, args.desde_lote)


if __name__ == "__main__":
    main()
